//! Service vsock global (refactor propre étape 5B, multi-session étape 6f).
//!
//! Ce module implémente le **modèle A** discuté : le VsockCapturer démarre
//! au boot du proxy (une fois, avant le serveur QUIC), et **reste en écoute
//! en permanence**. L'agent peut se connecter à tout moment, sans dépendre
//! de la présence d'un client.
//!
//! ## Étape 6f — Multi-session
//!
//! Avant : `frames_rx` était un `mpsc::Receiver` mono-consommateur, "pris"
//! une seule fois par `take_frames_receiver()`. Après une déconnexion
//! client, aucune nouvelle session ne pouvait recevoir de frames — il
//! fallait redémarrer tout le proxy.
//!
//! Après : le flux de frames passe par un canal `broadcast`. Une tâche de
//! fan-out interne consomme en continu le mpsc unique alimenté par le
//! VsockCapturer et rediffuse chaque frame vers ce canal broadcast. Chaque
//! nouvelle session cliente s'abonne indépendamment (`subscribe_frames()`
//! ou `subscribe_frames_as_mpsc()`), sans affecter les sessions passées ou
//! futures. La fin d'une session ne consomme plus la seule chance d'avoir
//! des frames pour la suivante.
//!
//! `subscribe_frames_as_mpsc()` adapte le flux broadcast en un
//! `mpsc::Receiver<RawFrame>` classique, pour rester compatible avec
//! `EncoderPipeline::start()` sans avoir à toucher au reste du pipeline
//! d'encodage.
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
//!         │         └──► frames_tx (mpsc, 1 seul consommateur : le fan-out)
//!         │                                        │
//!         │                                        ▼
//!         │                          tâche de fan-out (unique, permanente)
//!         │                                        │
//!         │                                        ▼
//!         │                       frames_broadcast (broadcast::Sender)
//!         │                                        │
//!         └── SERVICE (Arc statique) ───────────────┤
//!                                                   │
//!    [stream/mod.rs] session #1 ── subscribe_frames_as_mpsc() ──┤
//!    [stream/mod.rs] session #2 ── subscribe_frames_as_mpsc() ──┤  (indépendantes)
//!    [stream/mod.rs] session #N ── subscribe_frames_as_mpsc() ──┘
//! ```

use anyhow::{anyhow, Result};
use once_cell::sync::OnceCell;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{Capturer, CapturerCapabilities, RawFrame};

/// Taille du canal mpsc interne entre le VsockCapturer et la tâche de fan-out.
const FRAMES_CHANNEL_SIZE: usize = 8;

/// Capacité du canal broadcast : nombre de frames qu'un abonné lent peut
/// accumuler en retard avant de commencer à en perdre. Pour de la vidéo
/// temps réel, perdre une frame ancienne est préférable à bloquer le
/// pipeline — comportement volontaire, pas un bug.
const BROADCAST_CHANNEL_SIZE: usize = 8;

/// Le service singleton. Instancié une fois au boot du proxy, réutilisé
/// par toutes les sessions.
pub struct VsockService {
    /// Capacités annoncées par le capturer vsock. Utilisées par la session
    /// pour configurer l'encodeur (résolution, format).
    caps: CapturerCapabilities,

    /// Diffuse chaque `RawFrame` reçue de l'agent à tous les abonnés actifs.
    /// Voir la doc de module pour le détail du mécanisme multi-session.
    frames_broadcast: broadcast::Sender<RawFrame>,

    /// Émetteur pour relayer les InputBatch (JSON sérialisé) vers l'agent.
    /// Le VsockCapturer les reçoit et les envoie sur le canal vsock au format
    /// AgentMessage::Inputs.
    inputs_tx: mpsc::Sender<Vec<u8>>,

    /// Token de shutdown global du service (au moment où le proxy s'arrête).
    _shutdown: CancellationToken,

    /// Handle de la tâche du capturer.
    _capturer_handle: tokio::task::JoinHandle<Result<()>>,

    /// Handle de la tâche de fan-out (mpsc unique → broadcast N abonnés).
    _fanout_handle: tokio::task::JoinHandle<()>,
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

        info!(port, fps_limit, "démarrage du VsockService global (modèle A, multi-session)");

        // Créer le capturer vsock.
        let capturer = super::vsock::VsockCapturer::new(port)?;
        let caps = capturer.capabilities().clone();
        let inputs_tx = capturer.inputs_tx();

        // Canal interne unique : VsockCapturer → tâche de fan-out.
        let (frames_tx, mut frames_rx) = mpsc::channel::<RawFrame>(FRAMES_CHANNEL_SIZE);

        // Démarrer le capturer.
        let shutdown = CancellationToken::new();
        let cap_handle = capturer.clone().start(frames_tx, fps_limit, shutdown.clone());

        // Canal broadcast : fan-out → sessions clientes (N abonnés, présents
        // ou futurs). Le récepteur initial retourné par `channel()` n'est
        // utile à personne ici (chaque session fera son propre `subscribe`),
        // on le laisse simplement hors de portée.
        let (broadcast_tx, _unused_initial_rx) =
            broadcast::channel::<RawFrame>(BROADCAST_CHANNEL_SIZE);

        // Tâche de fan-out : consomme le mpsc unique alimenté par l'agent,
        // republie chaque frame sur le canal broadcast. Tourne en continu
        // pendant toute la vie du proxy, indépendamment des sessions
        // clientes qui vont et viennent.
        let fanout_tx = broadcast_tx.clone();
        let fanout_shutdown = shutdown.clone();
        let fanout_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = fanout_shutdown.cancelled() => {
                        info!("fan-out vsock : arrêt (shutdown du proxy)");
                        break;
                    }
                    frame = frames_rx.recv() => {
                        match frame {
                            Some(f) => {
                                // send() échoue seulement s'il n'y a aucun
                                // abonné actuellement — normal entre deux
                                // sessions clientes, sans conséquence : on
                                // ignore simplement l'erreur.
                                let _ = fanout_tx.send(f);
                            }
                            None => {
                                info!(
                                    "fan-out vsock : canal source fermé \
                                     (agent déconnecté ou capturer arrêté)"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        });

        // Construire le service.
        let service = Arc::new(VsockService {
            caps,
            frames_broadcast: broadcast_tx,
            inputs_tx,
            _shutdown: shutdown,
            _capturer_handle: cap_handle,
            _fanout_handle: fanout_handle,
        });

        // Stocker dans le singleton.
        SERVICE
            .set(service.clone())
            .map_err(|_| anyhow!("VsockService déjà initialisé (race condition ?)"))?;

        info!("VsockService démarré et prêt à recevoir des connexions agent (multi-session)");
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

    /// Handle pour envoyer des InputBatch (JSON sérialisé) à l'agent.
    /// Le stream serveur (côté proxy) utilise ce handle pour relayer les
    /// entrées reçues du client vers la VM via vsock.
    pub fn inputs_tx(&self) -> mpsc::Sender<Vec<u8>> {
        self.inputs_tx.clone()
    }

    /// S'abonne au flux de frames pour une nouvelle session cliente.
    ///
    /// Étape 6f : contrairement à l'ancien `take_frames_receiver()`, cette
    /// méthode peut être appelée autant de fois que nécessaire — chaque
    /// appel retourne un récepteur broadcast indépendant. La fin d'une
    /// session (drop du récepteur) n'affecte ni les sessions passées ni
    /// les futures.
    pub fn subscribe_frames(&self) -> broadcast::Receiver<RawFrame> {
        self.frames_broadcast.subscribe()
    }

    /// Comme `subscribe_frames()`, mais adapte le flux en un
    /// `mpsc::Receiver<RawFrame>` classique pour rester compatible avec
    /// `EncoderPipeline::start()` sans modification de ce dernier.
    ///
    /// Spawn une tâche d'adaptation dédiée à la session appelante ; elle se
    /// termine proprement quand `shutdown` est déclenché, quand la session
    /// cliente ferme son côté du canal, ou quand la source broadcast se
    /// ferme (agent déconnecté / proxy en arrêt).
    pub fn subscribe_frames_as_mpsc(
        &self,
        shutdown: CancellationToken,
    ) -> mpsc::Receiver<RawFrame> {
        let mut broadcast_rx = self.subscribe_frames();
        let (adapt_tx, adapt_rx) = mpsc::channel::<RawFrame>(FRAMES_CHANNEL_SIZE);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        break;
                    }
                    frame = broadcast_rx.recv() => {
                        match frame {
                            Ok(f) => {
                                if adapt_tx.send(f).await.is_err() {
                                    // La session cliente a fermé son côté :
                                    // fin de session normale.
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!(
                                    skipped = n,
                                    "session vsock en retard : frames perdues \
                                     (normal en cas de ralentissement ponctuel \
                                     du décodage côté client)"
                                );
                                continue;
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                info!(
                                    "flux broadcast fermé \
                                     (agent déconnecté ou proxy en arrêt)"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        });

        adapt_rx
    }
}
