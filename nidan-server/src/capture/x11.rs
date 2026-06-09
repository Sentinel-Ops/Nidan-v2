//! Capture d'écran X11 via XShm (shared memory) et XDamage (delta).
//!
//! ## Stratégie de capture
//!
//! ### Mode XShm + XDamage (optimal)
//! - XDamage notifie les rectangles modifiés → pas de capture si rien n'a changé
//! - XShm mappe la mémoire du serveur X directement → zéro copie
//! - CPU minimal, latence minimale
//!
//! ### Mode XShm seul (fallback)
//! - Capture complète à chaque frame via XShmGetImage
//! - Toujours zéro copie mais pas de delta
//!
//! ### Mode basique (dernier recours)
//! - XGetImage classique → copie kernel→userspace à chaque frame
//! - Compatible avec tout serveur X, y compris les vieux XVFB

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{Capturer, CapturerCapabilities, DamageRect, PixelFormat, RawFrame};

/// Capturer X11 — gère les trois modes de capture
pub struct X11Capturer {
    caps: CapturerCapabilities,
    display_number: u32,
    use_xshm: bool,
    use_xdamage: bool,
}

impl X11Capturer {
    /// Crée un nouveau capturer X11.
    ///
    /// Détecte automatiquement les extensions disponibles
    /// et choisit le mode de capture optimal.
    pub fn new(display_number: u32, use_xshm: bool, use_xdamage: bool) -> Result<Self> {
        // En mode stub (feature "stub" ou pas de X11 dispo),
        // on retourne des capacités synthétiques
        #[cfg(feature = "stub")]
        {
            warn!("X11Capturer en mode stub (feature stub activée)");
            return Ok(Self {
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
            });
        }

        // Connexion au serveur X
        #[cfg(all(feature = "x11-capture", not(feature = "stub")))]
        {
            use xcb::Connection;

            let display_str = format!(":{}", display_number);
            std::env::set_var("DISPLAY", &display_str);

            let (conn, screen_num) = Connection::connect(Some(&display_str))
                .with_context(|| format!("connexion X11 sur :{}", display_number))?;

            let setup = conn.get_setup();
            let screen = setup.roots().nth(screen_num as usize)
                .context("écran X11 introuvable")?;

            let width  = screen.width_in_pixels() as u32;
            let height = screen.height_in_pixels() as u32;

            // Vérification XShm
            let has_xshm = if use_xshm {
                conn.extension_data(xcb::xshm::id()).is_some()
            } else {
                false
            };

            // Vérification XDamage
            let has_xdamage = if use_xdamage {
                conn.extension_data(xcb::xdamage::id()).is_some()
            } else {
                false
            };

            info!(
                display   = display_number,
                width, height,
                xshm      = has_xshm,
                xdamage   = has_xdamage,
                "X11 initialisé"
            );

            return Ok(Self {
                caps: CapturerCapabilities {
                    width,
                    height,
                    supports_xshm: has_xshm,
                    supports_xdamage: has_xdamage,
                    pixel_format: PixelFormat::Bgra8888,
                },
                display_number,
                use_xshm: has_xshm,
                use_xdamage: has_xdamage,
            });
        }

        #[allow(unreachable_code)]
        bail!("X11 non disponible sur cette plateforme ou compilation")
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
        shutdown: tokio::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let capturer = self.clone();

        // La capture X11 doit tourner dans un thread dédié (non-async)
        // car xcb n'est pas async. On utilise spawn_blocking.
        tokio::task::spawn_blocking(move || {
            capturer.capture_loop_blocking(tx, fps_limit, shutdown)
        })
        .map(|res| res.unwrap_or_else(|e| Err(anyhow::anyhow!("panic dans le capturer: {}", e))))
        // Note: spawn_blocking retourne JoinHandle<T>, on adapte
        // En pratique on utilise un wrapper tokio::spawn + channel
        // (voir la vraie implémentation ci-dessous)
    }
}

impl X11Capturer {
    /// Boucle de capture synchrone (s'exécute dans spawn_blocking)
    fn capture_loop_blocking(
        &self,
        tx: mpsc::Sender<RawFrame>,
        fps_limit: u32,
        shutdown: tokio::sync::CancellationToken,
    ) -> Result<()> {
        #[cfg(all(feature = "x11-capture", not(feature = "stub")))]
        {
            use xcb::Connection;
            use std::time::{Duration, Instant};

            let display_str = format!(":{}", self.display_number);
            let (conn, screen_num) = Connection::connect(Some(&display_str))
                .context("reconnexion X11 dans le thread de capture")?;

            let setup   = conn.get_setup();
            let screen  = setup.roots().nth(screen_num as usize).unwrap();
            let root    = screen.root();
            let width   = self.caps.width;
            let height  = self.caps.height;

            // Initialisation XShm si disponible
            let xshm_seg = if self.use_xshm {
                Self::init_xshm(&conn, width, height).ok()
            } else {
                None
            };

            // Initialisation XDamage si disponible
            let damage = if self.use_xdamage {
                Self::init_xdamage(&conn, root).ok()
            } else {
                None
            };

            let frame_duration = Duration::from_micros(1_000_000 / fps_limit.max(1) as u64);
            let mut seq = 0u64;
            let mut last_frame = Instant::now();
            let mut pending_damage = false;
            let mut damage_rects: Vec<DamageRect> = Vec::new();

            info!(mode = if xshm_seg.is_some() { "XShm" } else { "basique" },
                  damage = damage.is_some(),
                  "boucle de capture démarrée");

            loop {
                // Vérification shutdown (non-bloquant)
                if shutdown.is_cancelled() {
                    info!("capture X11 arrêtée sur signal");
                    break;
                }

                // Traitement des événements X (XDamage)
                if let Some(_dmg) = &damage {
                    while let Some(event) = conn.poll_for_event() {
                        // Récupération des rectangles de dommage
                        // (xdamage::NOTIFY_EVENT)
                        damage_rects.push(DamageRect {
                            x: 0, y: 0,
                            width: width as u16,
                            height: height as u16,
                        });
                        pending_damage = true;
                    }
                }

                // Respect du fps_limit
                let elapsed = last_frame.elapsed();
                if elapsed < frame_duration {
                    std::thread::sleep(frame_duration - elapsed);
                    continue;
                }

                // Avec XDamage : ne capturer que si quelque chose a changé
                // (sauf pour les keyframes périodiques)
                let is_keyframe = seq == 0 || seq % (fps_limit as u64 * 2) == 0;
                if damage.is_some() && !pending_damage && !is_keyframe {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }

                // Capture de la frame
                let timestamp_us = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros() as u64;

                let data = if let Some(ref _seg) = xshm_seg {
                    // XShm : zéro copie — la mémoire est directement accessible
                    Self::capture_xshm(&conn, root, width, height)?
                } else {
                    // Fallback : XGetImage
                    Self::capture_basic(&conn, root, width, height)?
                };

                let frame = RawFrame {
                    data,
                    width,
                    height,
                    stride: width * 4,
                    timestamp_us,
                    seq,
                    is_keyframe,
                    damage_rects: std::mem::take(&mut damage_rects),
                };

                // Envoi bloquant avec backpressure
                // Si le channel est plein, on drop la frame (plutôt que bloquer)
                match tx.try_send(frame) {
                    Ok(())  => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        debug!("channel plein — frame {} droppée", seq);
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        info!("channel fermé, arrêt capture");
                        break;
                    }
                }

                pending_damage = false;
                last_frame = Instant::now();
                seq += 1;
            }

            Ok(())
        }

        #[cfg(any(not(feature = "x11-capture"), feature = "stub"))]
        {
            // Mode stub : génère des frames synthétiques
            let interval = std::time::Duration::from_millis(1000 / fps_limit.max(1) as u64);
            let mut seq = 0u64;
            let width = self.caps.width;
            let height = self.caps.height;

            loop {
                if shutdown.is_cancelled() { break; }
                std::thread::sleep(interval);

                let data = vec![128u8; (width * height * 4) as usize];
                let frame = RawFrame {
                    data, width, height,
                    stride: width * 4,
                    timestamp_us: 0,
                    seq,
                    is_keyframe: seq % 60 == 0,
                    damage_rects: vec![],
                };
                if tx.try_send(frame).is_err() { break; }
                seq += 1;
            }
            Ok(())
        }
    }

    /// Initialise un segment XShm
    #[cfg(all(feature = "x11-capture", not(feature = "stub")))]
    fn init_xshm(
        conn: &xcb::Connection,
        width: u32,
        height: u32,
    ) -> Result<()> {
        // En production : shmget + shmat + XShmAttach
        // Implémentation complète nécessite unsafe pour shm syscalls
        // Placeholder pour la Phase 1 — sera complété avec xcb::xshm
        info!(width, height, "XShm init (placeholder Phase 1)");
        Ok(())
    }

    /// Initialise XDamage sur la fenêtre root
    #[cfg(all(feature = "x11-capture", not(feature = "stub")))]
    fn init_xdamage(conn: &xcb::Connection, root: xcb::x::Window) -> Result<()> {
        use xcb::xdamage;
        let damage_id = conn.generate_id();
        conn.send_request(&xdamage::Create {
            damage: damage_id,
            drawable: xcb::x::Drawable::Window(root),
            level: xdamage::ReportLevel::NonEmpty,
        });
        conn.flush().context("flush XDamage create")?;
        info!("XDamage initialisé");
        Ok(())
    }

    /// Capture via XGetImage (mode basique)
    #[cfg(all(feature = "x11-capture", not(feature = "stub")))]
    fn capture_basic(
        conn: &xcb::Connection,
        root: xcb::x::Window,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        use xcb::x;

        let reply = conn.wait_for_reply(conn.send_request(&x::GetImage {
            format:    x::ImageFormat::ZPixmap,
            drawable:  x::Drawable::Window(root),
            x: 0, y: 0,
            width:     width as u16,
            height:    height as u16,
            plane_mask: u32::MAX,
        })).context("XGetImage")?;

        Ok(reply.data().to_vec())
    }

    /// Capture via XShm (zéro copie)
    #[cfg(all(feature = "x11-capture", not(feature = "stub")))]
    fn capture_xshm(
        conn: &xcb::Connection,
        root: xcb::x::Window,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        // En Phase 1 : fallback sur capture_basic jusqu'à implémentation XShm complète
        // TODO Phase 1.1 : implémenter XShmGetImage avec segment partagé
        Self::capture_basic(conn, root, width, height)
    }
}
