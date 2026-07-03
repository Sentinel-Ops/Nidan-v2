//! Service vsock global (refactor propre étape 5B).
//!
//! Ce module implémente le **modèle A** discuté : le VsockCapturer démarre
//! au boot du proxy (une fois, avant le serveur QUIC), et **reste en écoute
//! en permanence**. L'agent peut se connecter à tout moment, sans dépendre
//! de la présence d'un client.
//!
//! Quand une session client arrive, elle **s'abonne** au service pour
//! récupérer les frames en cours. Ce n'est pas un broadcast : dans cette
//! version simple, on est mono-session (un seul client à la fois consomme
//! les frames). Une évolution vers multi-client viendra si besoin.
//!
//! ## Architecture
//!
//! ```text
//!    [main.rs]  démarrage
//!         │
//!         ▼
//!    VsockService::start(port)
//!         │
//!         ├──► VsockCapturer.start()  ──► écoute vsock permanente
//!         │         │
//!         │         └──► frames_tx sur canal mpsc ─┐
//!         │                                        │
//!         │                                        ▼
//!         │                                  frames_rx (dans un Mutex)
//!         │                                        │
//!         └── SERVICE (Arc statique) ──────────────┤
//!                                                  │
//!    [stream/mod.rs] nouvelle session ─────────────┤
//!         session.take_frames_receiver() ──────────┘
//! ```

use anyhow::{anyhow, Result};
use once_cell::sync::OnceCell;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{Capturer, CapturerCapabilities, RawFrame};

/// Taille du canal de frames entre le capturer et la session cliente.
const FRAMES_CHANNEL_SIZE: usize = 8;

/// Le service singleton. Instancié une fois au boot du proxy, réutilisé
/// par toutes les sessions.
pub struct VsockService {
    /// Capacités annoncées par le capturer vsock. Utilisées par la session
    /// pour configurer l'encodeur (résolution, format).
    caps: CapturerCapabilities,

    /// Le récepteur de frames, dans un Mutex pour pouvoir le "prendre" à
    /// chaque nouvelle session. Une fois pris, il faut le remettre à la
    /// fin de session (drop propre) ou en recréer un.
    frames_rx: Arc<Mutex<Option<mpsc::Receiver<RawFrame>>>>,

    /// Token de shutdown global du service (au moment où le proxy s'arrête).
    _shutdown: CancellationToken,

    /// Handle de la tâche du capturer.
    _capturer_handle: tokio::task::JoinHandle<Result<()>>,
}

/// Le singleton global. Créé une seule fois par `start()`.
static SERVICE: OnceCell<Arc<VsockService>> = OnceCell::new();

impl VsockService {
    /// Démarre le service vsock. À appeler UNE FOIS au boot du proxy.
    /// Idempotent : appels suivants retournent le service existant.
    ///
    /// - `port` : port vsock d'écoute côté hôte (défaut : 6100).
    /// - `fps_limit` : fps max transmis au capturer.
    pub fn start(port: u32, fps_limit: u32) -> Result<Arc<Self>> {
        if let Some(existing) = SERVICE.get() {
            info!("VsockService déjà démarré, réutilisation");
            return Ok(existing.clone());
        }

        info!(port, fps_limit, "démarrage du VsockService global (modèle A)");

        // Créer le capturer vsock.
        let capturer = super::vsock::VsockCapturer::new(port)?;
        let caps = capturer.capabilities().clone();

        // Créer les channels frames.
        let (frames_tx, frames_rx) = mpsc::channel::<RawFrame>(FRAMES_CHANNEL_SIZE);

        // Démarrer le capturer.
        let shutdown = CancellationToken::new();
        let cap_handle = capturer.clone().start(frames_tx, fps_limit, shutdown.clone());

        // Construire le service.
        let service = Arc::new(VsockService {
            caps,
            frames_rx: Arc::new(Mutex::new(Some(frames_rx))),
            _shutdown: shutdown,
            _capturer_handle: cap_handle,
        });

        // Stocker dans le singleton.
        SERVICE
            .set(service.clone())
            .map_err(|_| anyhow!("VsockService déjà initialisé (race condition ?)"))?;

        info!("VsockService démarré et prêt à recevoir des connexions agent");
        Ok(service)
    }

    /// Récupère le service (ou None s'il n'a pas été démarré).
    pub fn get() -> Option<Arc<Self>> {
        SERVICE.get().cloned()
    }

    /// Capacités du capturer (résolution, format).
    pub fn capabilities(&self) -> &CapturerCapabilities {
        &self.caps
    }

    /// Prend le récepteur de frames pour une session cliente.
    /// Renvoie None si une autre session est déjà en cours (mono-session).
    ///
    /// Note pour l'étape 5B : mono-session par vie du proxy. Après une
    /// déconnexion client, la session suivante ne recevra pas de nouvelles
    /// frames (le récepteur a été consommé). Pour retester, redémarrer
    /// le proxy. Le multi-session viendra dans un fix ultérieur.
    pub async fn take_frames_receiver(&self) -> Option<mpsc::Receiver<RawFrame>> {
        let mut guard = self.frames_rx.lock().await;
        if guard.is_none() {
            warn!(
                "VsockService : récepteur de frames déjà consommé par une session \
                 précédente. Redémarrer le proxy pour une nouvelle session."
            );
        }
        guard.take()
    }
}
