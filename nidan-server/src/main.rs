//! # nidan-server
//!
//! Composant serveur NIDAN. Tourne dans la VM isolée, capture le display
//! X11/Windows, encode et transmet le flux vidéo chiffré via QUIC.

#![forbid(unsafe_code)]

use anyhow::Context;
use tracing::{error, info};

mod capture;
mod input;
mod config;
mod encoder;
mod session;
mod session_token;
mod stream;

use config::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nidan_common::logging::init("nidan-server");

    // Provider crypto rustls (requis avant tout usage TLS)
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    // Config depuis variable d'env ou chemin par défaut
    let config_path = std::env::var("NIDAN_SERVER_CONFIG")
        .unwrap_or_else(|_| "/etc/nidan-server.toml".to_string());

    info!(version = env!("CARGO_PKG_VERSION"), config = %config_path, "nidan-server démarrage");

    let cfg = ServerConfig::load(&config_path)
        .with_context(|| format!("chargement config: {config_path}"))?;

    cfg.validate().context("configuration invalide")?;

    let disp_num = std::env::var("NIDAN_DISPLAY")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(cfg.capture.display_number);

    info!("configuration chargee: bind={}, display={}, codec={}", cfg.network.bind_addr, disp_num, cfg.video.codec);

    let server = stream::QuicServer::new(cfg, disp_num).await
        .context("initialisation serveur QUIC")?;

    info!("serveur NIDAN prêt, en attente de connexions");

    match server.run().await {
        Ok(()) => info!("serveur arrêté proprement"),
        Err(e) => { error!(error = %e, "erreur fatale"); return Err(e); }
    }

    Ok(())
}
