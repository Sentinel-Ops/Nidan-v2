//! Presse-papier local du poste client — backend Wayland ou X11.
//!
//! Quand le client reçoit un presse-papier du serveur (sens serveur→client),
//! il doit le rendre collable (Ctrl+V) sur le poste local. Selon la session
//! graphique de l'utilisateur, le mécanisme diffère :
//!   - **Wayland** : protocole `ext-data-control`/`wlr-data-control` via le
//!     crate `wl-clipboard-rs` (feature `wayland-clipboard`).
//!   - **X11** : possession de la sélection CLIPBOARD via `x11rb`
//!     (feature `x11-clipboard`).
//!
//! Le backend est choisi automatiquement à l'exécution : `WAYLAND_DISPLAY`
//! prioritaire, sinon `DISPLAY`. Si aucun backend n'est compilé/disponible,
//! le presse-papier reçu est simplement journalisé (best-effort, sans échec).

use tracing::{debug, info, warn};

/// Presse-papier local unifié. Selon l'environnement, délègue au backend
/// Wayland ou X11. `set_content` est idempotent et non bloquant pour l'appelant.
pub enum LocalClipboard {
    #[cfg(feature = "wayland-clipboard")]
    Wayland,
    #[cfg(feature = "x11-clipboard")]
    X11(crate::clipboard_x11::ClipboardOwner),
    /// Aucun backend actif : on journalise seulement.
    Noop,
}

impl LocalClipboard {
    /// Détecte la session graphique et démarre le backend approprié.
    ///
    /// Ordre de préférence : Wayland (si `WAYLAND_DISPLAY`), puis X11 (si
    /// `DISPLAY`). Retourne toujours une instance (au pire `Noop`).
    pub fn detect_and_start() -> Self {
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let has_x11 = std::env::var_os("DISPLAY").is_some();

        // Wayland prioritaire (session moderne).
        #[cfg(feature = "wayland-clipboard")]
        if has_wayland {
            info!("presse-papier local : backend Wayland (wl-clipboard)");
            return LocalClipboard::Wayland;
        }

        // X11 (ou XWayland).
        #[cfg(feature = "x11-clipboard")]
        if has_x11 {
            match crate::clipboard_x11::ClipboardOwner::start_default() {
                Ok(owner) => {
                    info!("presse-papier local : backend X11 (sélection CLIPBOARD)");
                    return LocalClipboard::X11(owner);
                }
                Err(e) => warn!(error = %e, "backend X11 indisponible"),
            }
        }

        let _ = (has_wayland, has_x11);
        warn!("presse-papier local indisponible (ni Wayland ni X11 utilisable) — journalisation seule");
        LocalClipboard::Noop
    }

    /// Définit le contenu collable localement (déjà filtré par la politique).
    pub fn set_content(&self, content: &[u8]) {
        match self {
            #[cfg(feature = "wayland-clipboard")]
            LocalClipboard::Wayland => set_wayland(content),
            #[cfg(feature = "x11-clipboard")]
            LocalClipboard::X11(owner) => owner.set_content(content),
            LocalClipboard::Noop => {
                debug!(bytes = content.len(), "presse-papier local (noop) — non collable");
            }
        }
    }
}

/// Écrit le contenu dans le presse-papier Wayland via wl-clipboard-rs.
/// L'opération sert le contenu jusqu'à ce qu'un autre client prenne la main.
#[cfg(feature = "wayland-clipboard")]
fn set_wayland(content: &[u8]) {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    let opts = Options::new();
    match opts.copy(Source::Bytes(content.to_vec().into()), MimeType::Autodetect) {
        Ok(()) => debug!(bytes = content.len(), "presse-papier Wayland mis à jour"),
        Err(e) => warn!(error = %e, "échec écriture presse-papier Wayland"),
    }
}
