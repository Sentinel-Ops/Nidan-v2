//! Capture d'écran X11.
//!
//! Deux modes :
//! - **Réel** (feature `x11-capture`) : capture via x11rb (XGetImage)
//! - **Stub** (par défaut) : frames synthétiques pour tests sans serveur X

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{Capturer, CapturerCapabilities, PixelFormat, RawFrame};

/// Capturer X11
pub struct X11Capturer {
    caps: CapturerCapabilities,
    display_number: u32,
    #[allow(dead_code)]
    use_xshm: bool,
    #[allow(dead_code)]
    use_xdamage: bool,
}

impl X11Capturer {
    /// Crée un nouveau capturer X11, détecte les extensions disponibles.
    pub fn new(display_number: u32, use_xshm: bool, use_xdamage: bool) -> Result<Self> {
        #[cfg(feature = "x11-capture")]
        {
            Self::new_real(display_number, use_xshm, use_xdamage)
        }

        #[cfg(not(feature = "x11-capture"))]
        {
            let _ = (use_xshm, use_xdamage);
            warn!("X11Capturer en mode stub (feature x11-capture désactivée)");
            Ok(Self {
                caps: CapturerCapabilities {
                    width: 1920,
                    height: 1080,
                    supports_xshm: false,
                    supports_xdamage: false,
                    pixel_format: PixelFormat::Bgra8888,
                },
                display_number,
                use_xshm: false,
                use_xdamage: false,
            })
        }
    }

    #[cfg(feature = "x11-capture")]
    fn new_real(display_number: u32, use_xshm: bool, use_xdamage: bool) -> Result<Self> {
        use x11rb::connection::{Connection, RequestConnection as _};
        use x11rb::protocol::xproto::ConnectionExt as _;

        let display_str = format!(":{}", display_number);
        let (conn, screen_num) = x11rb::connect(Some(&display_str))
            .with_context(|| format!("connexion X11 sur {}", display_str))?;

        let screen = &conn.setup().roots[screen_num];
        let width = screen.width_in_pixels as u32;
        let height = screen.height_in_pixels as u32;
        let root = screen.root;

        let has_xshm = if use_xshm {
            conn.extension_information(x11rb::protocol::shm::X11_EXTENSION_NAME)
                .ok().flatten().is_some()
        } else { false };

        let has_xdamage = if use_xdamage {
            conn.extension_information(x11rb::protocol::damage::X11_EXTENSION_NAME)
                .ok().flatten().is_some()
        } else { false };

        let _geom = conn.get_geometry(root)
            .context("get_geometry root")?
            .reply()
            .context("get_geometry reply")?;

        info!(
            display = display_number, width, height,
            xshm = has_xshm, xdamage = has_xdamage,
            "X11 initialisé (capture réelle)"
        );

        Ok(Self {
            caps: CapturerCapabilities {
                width, height,
                supports_xshm: has_xshm,
                supports_xdamage: has_xdamage,
                pixel_format: PixelFormat::Bgra8888,
            },
            display_number,
            use_xshm: has_xshm,
            use_xdamage: has_xdamage,
        })
    }
}

impl Capturer for X11Capturer {
    fn capabilities(&self) -> &CapturerCapabilities {
        &self.caps
    }

    fn start(
        self: Arc<Self>,
        tx: mpsc::Sender<RawFrame>,
        fps_limit: u32,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let capturer = self.clone();
        tokio::task::spawn_blocking(move || {
            capturer.capture_loop_blocking(tx, fps_limit, shutdown)
        })
    }
}

impl X11Capturer {
    fn capture_loop_blocking(
        &self,
        tx: mpsc::Sender<RawFrame>,
        fps_limit: u32,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        #[cfg(feature = "x11-capture")]
        { self.capture_loop_real(tx, fps_limit, shutdown) }

        #[cfg(not(feature = "x11-capture"))]
        { self.capture_loop_stub(tx, fps_limit, shutdown) }
    }

    #[cfg(feature = "x11-capture")]
    fn capture_loop_real(
        &self,
        tx: mpsc::Sender<RawFrame>,
        fps_limit: u32,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        use std::time::{Duration, Instant};
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};

        let display_str = format!(":{}", self.display_number);
        let (conn, screen_num) = x11rb::connect(Some(&display_str))
            .context("reconnexion X11 dans le thread de capture")?;

        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let width = self.caps.width;
        let height = self.caps.height;

        let frame_duration = Duration::from_micros(1_000_000 / fps_limit.max(1) as u64);
        let keyframe_period = (fps_limit as u64 * 2).max(1);
        let mut seq = 0u64;
        let mut last_frame = Instant::now();

        info!(width, height, fps = fps_limit, "capture X11 réelle (XGetImage)");

        loop {
            if shutdown.is_cancelled() {
                info!("capture X11 arrêtée");
                break;
            }

            let elapsed = last_frame.elapsed();
            if elapsed < frame_duration {
                std::thread::sleep(frame_duration - elapsed);
            }
            last_frame = Instant::now();

            let reply = match conn.get_image(
                ImageFormat::Z_PIXMAP, root,
                0, 0, width as u16, height as u16, u32::MAX,
            ) {
                Ok(cookie) => match cookie.reply() {
                    Ok(r) => r,
                    Err(e) => { warn!(error = ?e, "get_image reply"); continue; }
                },
                Err(e) => { warn!(error = ?e, "get_image"); continue; }
            };

            let bpp = 4usize;
            let expected = (width * height) as usize * bpp;
            let mut data = reply.data;
            if data.len() < expected {
                warn!(got = data.len(), expected, "taille image inattendue");
                continue;
            }
            data.truncate(expected);
            // Forcer alpha = 255
            let mut i = 3;
            while i < data.len() { data[i] = 0xFF; i += 4; }

            let is_keyframe = seq == 0 || seq % keyframe_period == 0;
            let timestamp_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default().as_micros() as u64;

            let frame = RawFrame {
                data, width, height, stride: width * 4,
                timestamp_us, seq, is_keyframe, damage_rects: vec![],
            };

            match tx.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => debug!(seq, "frame droppée"),
                Err(mpsc::error::TrySendError::Closed(_)) => { info!("channel fermé"); break; }
            }
            seq += 1;
        }
        Ok(())
    }

    #[cfg(not(feature = "x11-capture"))]
    fn capture_loop_stub(
        &self,
        tx: mpsc::Sender<RawFrame>,
        fps_limit: u32,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        use std::time::Duration;
        let interval = Duration::from_millis(1000 / fps_limit.max(1) as u64);
        let mut seq = 0u64;
        let width = self.caps.width;
        let height = self.caps.height;

        info!(width, height, fps = fps_limit, "capture stub démarrée");

        loop {
            if shutdown.is_cancelled() { break; }
            std::thread::sleep(interval);

            let data: Vec<u8> = (0..width * height * 4)
                .map(|i| {
                    let pixel = i / 4; let channel = i % 4;
                    let x = pixel % width; let y = pixel / width;
                    match channel {
                        0 => ((x + seq as u32) % 256) as u8,
                        1 => ((y + seq as u32 / 2) % 256) as u8,
                        2 => (seq as u32 % 256) as u8,
                        _ => 255u8,
                    }
                }).collect();

            let frame = RawFrame {
                data, width, height, stride: width * 4,
                timestamp_us: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default().as_micros() as u64,
                seq, is_keyframe: seq % 60 == 0, damage_rects: vec![],
            };
            if tx.try_send(frame).is_err() { break; }
            seq += 1;
        }
        Ok(())
    }
}
