//! # nidan-proxy-encoder
//!
//! Composant NIDAN v2. Tourne sur l'HÔTE (pas dans la VM).
//!
//! Rôle :
//!   • reçoit des frames RGBA brutes depuis une source abstraite
//!     (aujourd'hui : StubCapturer pour la validation d'étape 3 ;
//!      demain : agent vsock à l'étape 5) ;
//!   • encode le flux en H.264 sur l'hôte (zone de confiance) ;
//!   • chiffre E2E (X25519 + ChaCha20-Poly1305) ;
//!   • expose un service QUIC + mTLS au client.
//!
//! C'est le cœur du modèle d'isolation Sanzu : l'encodeur est HORS de la
//! VM navigateur, donc une VM compromise ne peut pas forger un flux H.264
//! piégé vers le client.

#![forbid(unsafe_code)]

use anyhow::Context;
use tracing::{error, info};

mod capture;
mod input;
#[cfg(feature="remotedesktop-input")]
mod remote_desktop;
mod config;
mod encoder;
mod session;
mod session_token;
#[cfg(feature = "x11-capture")]
mod clipboard_x11;
mod stream;

use config::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nidan_common::logging::init("nidan-proxy-encoder");

    // Arrêt immédiat sur Ctrl+C (SIGINT) : termine le processus sans attendre
    // les threads bloquants (RemoteDesktop, boucles QUIC, etc.).
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl+C reçu — arrêt du proxy-encoder");
            std::process::exit(0);
        }
    });

    // Provider crypto rustls (requis avant tout usage TLS)
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    // Config depuis variable d'env ou chemin par défaut
    let config_path = std::env::var("NIDAN_PROXY_CONFIG")
        .unwrap_or_else(|_| "/etc/nidan-proxy-encoder.toml".to_string());

    info!(version = env!("CARGO_PKG_VERSION"), config = %config_path, "nidan-proxy-encoder démarrage");

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
