//! Injection des événements d'entrée (clavier/souris) dans le serveur X
//! via l'extension XTEST.
//!
//! Reçoit les `InputEvent` du client et les rejoue sur le display X de la VM.
//! Derrière la feature `x11-capture` (même dépendance x11rb que la capture).

use anyhow::{Context, Result};
use tracing::{debug, warn};

use nidan_proto::{InputBatch, InputEventPayload};

/// Types d'événements X (protocole X11)
#[cfg(feature = "x11-capture")]
mod x_event {
    pub const KEY_PRESS: u8     = 2;
    pub const KEY_RELEASE: u8   = 3;
    pub const BUTTON_PRESS: u8  = 4;
    pub const BUTTON_RELEASE: u8 = 5;
    pub const MOTION_NOTIFY: u8 = 6;
}

/// Injecteur d'événements dans le serveur X
pub struct InputInjector {
    #[cfg(feature = "x11-capture")]
    conn: x11rb::rust_connection::RustConnection,
    #[cfg(feature = "x11-capture")]
    root: u32,
    width: u32,
    height: u32,
    is_stub: bool,
    injected: u64,
    clipboard: Vec<u8>,
}

impl InputInjector {
    /// Crée un injecteur connecté au display X
    pub fn new(display_number: u32, width: u32, height: u32) -> Result<Self> {
        #[cfg(feature = "x11-capture")]
        {
            use x11rb::connection::{Connection, RequestConnection as _};

            let display_str = format!(":{}", display_number);
            let (conn, screen_num) = x11rb::connect(Some(&display_str))
                .with_context(|| format!("connexion X11 (input) sur {}", display_str))?;

            // Vérifier la présence de l'extension XTEST
            let has_xtest = conn
                .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
                .ok().flatten().is_some();
            if !has_xtest {
                anyhow::bail!("extension XTEST absente — injection impossible");
            }

            let root = conn.setup().roots[screen_num].root;
            tracing::info!(display = display_number, "injecteur d'inputs XTEST initialisé");

            Ok(Self { conn, root, width, height, is_stub: false, injected: 0, clipboard: Vec::new() })
        }

        #[cfg(not(feature = "x11-capture"))]
        {
            warn!("InputInjector en mode stub (feature x11-capture désactivée)");
            Ok(Self { width, height, is_stub: true, injected: 0, clipboard: Vec::new() })
        }
    }

    /// Injecte tous les événements d'un batch
    pub fn inject_batch(&mut self, batch: &InputBatch) -> Result<()> {
        for event in &batch.events {
            if let Err(e) = self.inject_one(event) {
                warn!(error = %e, seq = event.seq, "injection événement échouée");
            }
        }
        Ok(())
    }

    /// Injecte un seul événement
    fn inject_one(&mut self, event: &nidan_proto::InputEvent) -> Result<()> {
        if self.is_stub {
            self.injected += 1;
            debug!(seq = event.seq, type_ = event.event_type, "input (stub, non injecté)");
            return Ok(());
        }

        #[cfg(feature = "x11-capture")]
        {
            use x11rb::connection::Connection;
            use x11rb::protocol::xtest::ConnectionExt as _;

            match &event.event {
                Some(InputEventPayload::Key(k)) => {
                    // event_type : 1 = KeyDown, 2 = KeyUp
                    let x_type = if event.event_type == 1 {
                        x_event::KEY_PRESS
                    } else {
                        x_event::KEY_RELEASE
                    };
                    // Le scancode X = keycode + 8 (offset clavier X11 classique)
                    let detail = (k.keycode as u8).wrapping_add(8);
                    self.conn.xtest_fake_input(x_type, detail, 0, self.root, 0, 0, 0)
                        .context("xtest key")?;
                }
                Some(InputEventPayload::Mouse(m)) => {
                    match event.event_type {
                        // MouseMove : coordonnées normalisées [0,1] → pixels absolus
                        3 => {
                            let px = (m.x * self.width as f32) as i16;
                            let py = (m.y * self.height as f32) as i16;
                            self.conn.xtest_fake_input(
                                x_event::MOTION_NOTIFY, 0, 0, self.root, px, py, 0
                            ).context("xtest motion")?;
                        }
                        // MouseDown / MouseUp : button dans m.button (1=gauche,2=milieu,3=droit)
                        4 | 5 => {
                            let x_type = if event.event_type == 4 {
                                x_event::BUTTON_PRESS
                            } else {
                                x_event::BUTTON_RELEASE
                            };
                            let button = (m.button as u8).clamp(1, 5);
                            self.conn.xtest_fake_input(x_type, button, 0, self.root, 0, 0, 0)
                                .context("xtest button")?;
                        }
                        // MouseScroll : boutons 4 (haut) / 5 (bas)
                        6 => {
                            let button = if m.scroll_dy > 0.0 { 4u8 } else { 5u8 };
                            // press + release pour simuler un cran de molette
                            self.conn.xtest_fake_input(x_event::BUTTON_PRESS, button, 0, self.root, 0, 0, 0)
                                .context("xtest scroll press")?;
                            self.conn.xtest_fake_input(x_event::BUTTON_RELEASE, button, 0, self.root, 0, 0, 0)
                                .context("xtest scroll release")?;
                        }
                        _ => {}
                    }
                }
                None => {}
            }

            self.conn.flush().context("flush XTEST")?;
            self.injected += 1;
            debug!(seq = event.seq, "input injecté via XTEST");
            Ok(())
        }

        #[cfg(not(feature = "x11-capture"))]
        {
            let _ = event;
            Ok(())
        }
    }

    pub fn injected_count(&self) -> u64 {
        self.injected
    }

    /// Définit le contenu du presse-papier de la VM (sélection X CLIPBOARD).
    ///
    /// Le contenu a déjà été filtré par la politique avant d'arriver ici.
    /// Note : la prise de possession complète de la sélection X (réponse
    /// asynchrone aux `SelectionRequest`) est un raffinement ultérieur ;
    /// cette méthode valide la réception et conserve le contenu courant.
    pub fn set_clipboard(&mut self, content: &[u8]) -> Result<()> {
        self.clipboard = content.to_vec();
        debug!(bytes = content.len(), "presse-papier de la VM mis à jour");
        Ok(())
    }

    /// Dernier contenu de presse-papier reçu (après filtrage).
    pub fn clipboard(&self) -> &[u8] {
        &self.clipboard
    }
}
