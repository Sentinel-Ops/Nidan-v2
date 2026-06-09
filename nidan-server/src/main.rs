//! # nidan-server
//!
//! Composant serveur NIDAN. Tourne dans la VM isolée, capture le display
//! X11/Windows, encode et transmet le flux vidéo chiffré via QUIC.

#![forbid(unsafe_code)]

use anyhow::Context;
use tracing::{error, info};

mod capture;
mod config;
mod encoder;
mod session;
mod stream;

use config::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nidan_common::logging::init("nidan-server");

    // Config depuis variable d'env ou chemin par défaut
    let config_path = std::env::var("NIDAN_SERVER_CONFIG")
        .unwrap_or_else(|_| "/etc/nidan-server.toml".to_string());

    info!(version = env!("CARGO_PKG_VERSION"), config = %config_path, "nidan-server démarrage");

    let cfg = ServerConfig::load(&config_path)
        .with_context(|| format!("chargement config: {config_path}"))?;

    cfg.validate().context("configuration invalide")?;

    let display = std::env::var("NIDAN_DISPLAY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(cfg.capture.display_number);

    info!(bind = %cfg.network.bind_addr, display, codec = %cfg.video.codec, "configuration OK");

    let server = stream::QuicServer::new(cfg, display).await
        .context("initialisation serveur QUIC")?;

    info!("serveur NIDAN prêt, en attente de connexions");

    match server.run().await {
        Ok(()) => info!("serveur arrêté proprement"),
        Err(e) => { error!(error = %e, "erreur fatale"); return Err(e); }
    }

    Ok(())
}
