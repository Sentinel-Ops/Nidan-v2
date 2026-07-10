//! Abstraction de la capture d'écran.
//!
//! Fournit une interface unifiée (`ScreenCapture`) indépendante
//! de la plateforme (X11, Wayland, Windows DXGI).
//!
//! Architecture du pipeline de capture :
//! ```text
//! [Display] → [Capturer] → [RawFrame channel] → [Encodeur FFmpeg]
//!                ↑
//!           XDamage events (Linux)
//!           DXGI duplication (Windows)
//! ```

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub mod x11;

#[cfg(feature = "vsock-source")]
pub mod vsock;

#[cfg(feature = "vsock-source")]
pub mod vsock_service;
#[cfg(feature = "pipewire-capture")]
pub mod pipewire;

/// Une frame brute capturée — pixels non compressés
#[derive(Debug, Clone)]
pub struct RawFrame {
    /// Données BGRA ou RGB selon la plateforme
    pub data: Vec<u8>,
    /// Largeur en pixels
    pub width: u32,
    /// Hauteur en pixels
    pub height: u32,
    /// Stride (bytes par ligne), peut différer de width*4
    pub stride: u32,
    /// Timestamp de capture (µs depuis UNIX epoch)
    pub timestamp_us: u64,
    /// Numéro séquentiel de la frame (monotone)
    pub seq: u64,
    /// Indique si la frame est complète ou différentielle (XDamage)
    pub is_keyframe: bool,
    /// Rectangle(s) modifié(s) depuis la frame précédente (optionnel)
    pub damage_rects: Vec<DamageRect>,
}

/// Rectangle de dommage (région modifiée)
#[derive(Debug, Clone, Copy)]
pub struct DamageRect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// Capacités du capturer détectées à l'initialisation
#[derive(Debug, Clone)]
pub struct CapturerCapabilities {
    pub width: u32,
    pub height: u32,
    pub supports_xshm: bool,
    pub supports_xdamage: bool,
    pub pixel_format: PixelFormat,
}

/// Format de pixel des données brutes
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PixelFormat {
    Bgra8888,
    Rgba8888,
    Rgb888,
}

impl PixelFormat {
    /// Bytes par pixel
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelFormat::Bgra8888 | PixelFormat::Rgba8888 => 4,
            PixelFormat::Rgb888 => 3,
        }
    }

    /// Nom FFmpeg correspondant (pour lavf)
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            PixelFormat::Bgra8888 => "bgra",
            PixelFormat::Rgba8888 => "rgba",
            PixelFormat::Rgb888   => "rgb24",
        }
    }
}

/// Trait principal de capture d'écran
/// Implémenté par X11Capturer, WaylandCapturer, DxgiCapturer
pub trait Capturer: Send + Sync {
    /// Capacités détectées
    fn capabilities(&self) -> &CapturerCapabilities;

    /// Démarre la boucle de capture en arrière-plan.
    /// Les frames sont envoyées sur le channel `tx`.
    /// La boucle s'arrête quand `shutdown` est dropped.
    fn start(
        self: Arc<Self>,
        tx: mpsc::Sender<RawFrame>,
        fps_limit: u32,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>>;
}

/// Crée le capturer approprié pour la plateforme courante
pub fn create_capturer(
    backend: &str,
    display_number: u32,
    use_xshm: bool,
    use_xdamage: bool,
    portal_restore_token: Option<String>,
) -> Result<Arc<dyn Capturer>> {
    match backend {
        // Réception de frames depuis un agent NIDAN v2 par vsock (modèle Sanzu)
        #[cfg(feature = "vsock-source")]
        "vsock" => {
            let port = std::env::var("NIDAN_VSOCK_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(vsock::DEFAULT_VSOCK_PORT);
            info!(port, "initialisation capturer vsock (écoute côté hôte)");
            let capturer = vsock::VsockCapturer::new(port)?;
            return Ok(capturer);
        }
        #[cfg(not(feature = "vsock-source"))]
        "vsock" => {
            anyhow::bail!("capture vsock demandée mais la feature vsock-source n'est pas compilée");
        }
        // Capture Wayland via portail ScreenCast + PipeWire
        #[cfg(feature = "pipewire-capture")]
        "wayland" | "pipewire" => {
            info!("initialisation capturer Wayland (portail ScreenCast + PipeWire)");
            let capturer = pipewire::PipeWireCapturer::new(portal_restore_token)?;
            Ok(Arc::new(capturer))
        }
        #[cfg(not(feature = "pipewire-capture"))]
        "wayland" | "pipewire" => {
            anyhow::bail!("capture Wayland demandée mais la feature pipewire-capture n'est pas compilée");
        }
        // Capture X11 (XGetImage, session Xorg) — défaut
        _ => {
            #[cfg(target_os = "linux")]
            {
                let _ = portal_restore_token;
                info!(display_num = display_number, xshm = use_xshm, xdamage = use_xdamage, "initialisation capturer X11");
                let capturer = x11::X11Capturer::new(display_number, use_xshm, use_xdamage)?;
                Ok(Arc::new(capturer))
            }
            #[cfg(not(target_os = "linux"))]
            {
                warn!("plateforme non supportée — utilisation du capturer stub");
                Ok(Arc::new(StubCapturer::new()))
            }
        }
    }
}

/// Capturer stub pour tests et compilation cross-plateforme
pub struct StubCapturer {
    caps: CapturerCapabilities,
}

impl StubCapturer {
    pub fn new() -> Self {
        Self {
            caps: CapturerCapabilities {
                width: 1920,
                height: 1080,
                supports_xshm: false,
                supports_xdamage: false,
                pixel_format: PixelFormat::Bgra8888,
            },
        }
    }
}

impl Capturer for StubCapturer {
    fn capabilities(&self) -> &CapturerCapabilities {
        &self.caps
    }

    fn start(
        self: Arc<Self>,
        tx: mpsc::Sender<RawFrame>,
        fps_limit: u32,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            let interval_ms = 1000u64 / fps_limit.max(1) as u64;
            let mut seq = 0u64;
            let width = 1920u32;
            let height = 1080u32;

            info!("capturer stub démarré ({}x{} @ {}fps)", width, height, fps_limit);

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        info!("capturer stub arrêté");
                        break;
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(interval_ms)) => {
                        // Frame synthétique — dégradé qui change à chaque frame
                        let data: Vec<u8> = (0..width * height * 4)
                            .map(|i| {
                                let pixel = i / 4;
                                let channel = i % 4;
                                let x = pixel % width;
                                let y = pixel / width;
                                match channel {
                                    0 => ((x + seq as u32) % 256) as u8,      // B
                                    1 => ((y + seq as u32 / 2) % 256) as u8,  // G
                                    2 => (seq as u32 % 256) as u8,            // R
                                    _ => 255u8,                                // A
                                }
                            })
                            .collect();

                        let frame = RawFrame {
                            data,
                            width,
                            height,
                            stride: width * 4,
                            timestamp_us: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_micros() as u64,
                            seq,
                            is_keyframe: seq == 0 || seq % 60 == 0,
                            damage_rects: vec![],
                        };

                        if tx.send(frame).await.is_err() {
                            debug!("canal de frames fermé, arrêt du capturer stub");
                            break;
                        }
                        seq += 1;
                    }
                }
            }
            Ok(())
        })
    }
}
