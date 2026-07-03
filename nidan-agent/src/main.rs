//! # nidan-agent
//!
//! Composant NIDAN v2. Tourne DANS la VM Ubuntu (zone hostile potentielle).
//!
//! Rôle :
//!   • capture le bureau Wayland (via portail PipeWire/ScreenCast) ;
//!   • envoie des pixels bruts (RGBA/BGRA) à l'hôte via vsock ;
//!   • reçoit les entrées clavier/souris depuis le proxy et les injecte
//!     dans la session Wayland (portail RemoteDesktop).
//!
//! Il ne fait PAS :
//!   • d'encodage vidéo (c'est le proxy-encoder sur l'hôte qui encode) ;
//!   • de gestion de session ou de JWT (le broker + proxy s'en occupent) ;
//!   • de TLS/mTLS (vsock est un canal local hôte↔invité).
//!
//! C'est la brique complémentaire du proxy-encoder : ensemble, ils
//! réalisent le modèle Sanzu — encodeur hors VM, la VM ne peut pas
//! forger un flux vidéo piégé vers le client.

#![forbid(unsafe_code)]

use anyhow::Context;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

mod capture;
mod config;
mod input;
#[cfg(feature = "remotedesktop-input")]
mod remote_desktop;
mod vsock_link;

use config::AgentConfig;
// PixelFormat vient du proto v2 — le capture/mod.rs a un enum de même nom,
// donc on qualifie explicitement pour éviter toute confusion.
use nidan_proto::agent::PixelFormat as ProtoPixelFormat;
use vsock_link::ProxyCommand;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nidan_common::logging::init("nidan-agent");
    info!(version = env!("CARGO_PKG_VERSION"), "nidan-agent démarrage");

    // 1. Charger la config.
    let config_path = std::env::var("NIDAN_AGENT_CONFIG")
        .unwrap_or_else(|_| "/etc/nidan-agent.toml".to_string());
    info!(config = %config_path, "chargement de la config");
    let cfg = AgentConfig::load(std::path::Path::new(&config_path))
        .with_context(|| format!("chargement config: {}", config_path))?;

    // 2. Créer le capturer (stub ou pipewire selon la config).
    // Convention v1 : capture::create_capturer(&cfg.backend) retourne un
    // Arc<dyn Capturer>. On garde exactement le même pattern pour compat.
    // Signature v1 : create_capturer(backend, display_number, use_xshm, use_xdamage, restore_token).
    // Pour l'agent : display_number = 0, pas d'optim X11 (sur Wayland), pas de token de restauration.
    let capturer = capture::create_capturer(&cfg.capture.backend, 0, false, false, None)
        .with_context(|| format!("initialisation capturer '{}'", cfg.capture.backend))?;
    let caps = capturer.capabilities();
    info!(
        backend = %cfg.capture.backend,
        width  = caps.width,
        height = caps.height,
        pixel_format = ?caps.pixel_format,
        "capturer prêt"
    );

    // 3. Formats supportés — on annonce RGBA + BGRA, avec en tête celui que
    // le capturer local produit nativement (pour privilégier une session sans
    // conversion couleur si le proxy est d'accord).
    let native_first = match caps.pixel_format {
        capture::PixelFormat::Bgra8888 => vec![ProtoPixelFormat::Bgra8, ProtoPixelFormat::Rgba8],
        _                              => vec![ProtoPixelFormat::Rgba8, ProtoPixelFormat::Bgra8],
    };
    let supported_formats = native_first;

    // 4. Handshake vsock avec le proxy.
    let (stream, start) = vsock_link::connect_and_handshake(
        cfg.vsock.host_cid,
        cfg.vsock.port,
        supported_formats,
        caps.width,
        caps.height,
        cfg.capture.max_fps,
    )
    .await
    .context("handshake vsock avec le proxy")?;

    // 5. Format négocié par le proxy (depuis StartCapture).
    // Si le proxy demande un format qu'on ne peut pas fournir, on renvoie
    // ce qu'on a et le proxy s'adaptera (BGRA est plus fréquent sous
    // PipeWire, RGBA plus fréquent sous X11).
    let negotiated_format = ProtoPixelFormat::try_from(start.format)
        .unwrap_or(ProtoPixelFormat::Bgra8);
    info!(?negotiated_format, "format négocié pour la session");

    // 6. Démarrer la capture. Le capturer va pousser des RawFrame sur frames_tx.
    let (frames_tx, frames_rx) = mpsc::channel::<capture::RawFrame>(
        vsock_link::FRAME_QUEUE_SIZE_DEFAULT,
    );
    let shutdown = CancellationToken::new();
    let capture_handle = {
        let cap = capturer.clone();
        let sh = shutdown.clone();
        let fps = if start.target_fps > 0 { start.target_fps } else { cfg.capture.max_fps };
        cap.start(frames_tx, fps, sh)
    };

    // 7. Canal pour les commandes venant du proxy (Start/Stop/Inputs).
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ProxyCommand>(16);

    // 7bis. Ctrl+C → shutdown propre.
    let shutdown_signal = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("Ctrl+C reçu — arrêt propre de l'agent");
            shutdown_signal.cancel();
        }
    });

    // 8. Tâche : consommer les commandes du proxy (pour l'étape 4, on ne
    //    fait qu'observer ; l'intégration RemoteDesktop viendra à l'étape 5).
    let shutdown_cmd = shutdown.clone();
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                ProxyCommand::Stop(_) => {
                    info!("commande Stop reçue du proxy — arrêt");
                    shutdown_cmd.cancel();
                    break;
                }
                ProxyCommand::Start(s) => {
                    info!(
                        width = s.target_width,
                        height = s.target_height,
                        "commande Start (reconfiguration) reçue"
                    );
                    // À l'étape 5 : reconfigurer le capturer si nécessaire.
                }
                ProxyCommand::Inputs(_bytes) => {
                    // À l'étape 5 : décoder l'InputBatch et l'injecter via
                    // RemoteDesktop (ou X11 en fallback).
                }
            }
        }
    });

    // 9. Boucle principale : envoyer les frames sur vsock.
    if let Err(e) = vsock_link::run_session(
        stream,
        frames_rx,
        cmd_tx,
        negotiated_format,
        shutdown.clone(),
    )
    .await
    {
        error!(error = %e, "erreur pendant la session vsock");
    }

    // 10. Attente propre du capturer.
    shutdown.cancel();
    if let Err(e) = capture_handle.await {
        error!(error = %e, "erreur lors de l'arrêt du capturer");
    }
    info!("nidan-agent arrêté proprement");
    Ok(())
}
