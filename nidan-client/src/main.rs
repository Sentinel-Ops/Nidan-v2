//! # nidan-client
//!
//! Composant client NIDAN.
//! Se connecte au broker pour obtenir une session, puis stream le bureau
//! distant depuis nidan-server : décode le flux vidéo H.264/H.265/AV1,
//! l'affiche via SDL2/wgpu, et remonte clavier/souris/clipboard.
//!
//! ## Cycle de vie
//! 1. Chargement config + parsing CLI
//! 2. Connexion QUIC au broker → obtention session token + adresse serveur
//! 3. Connexion QUIC au serveur avec token
//! 4. Handshake proto (capacités décodeur, layout moniteurs)
//! 5. Boucle principale : réception VideoFrame → décode → affiche
//!    + remontée InputEvent → serveur
//! 6. Fermeture propre sur Ctrl+Q ou signal

#![forbid(unsafe_code)]

use anyhow::Context;
use clap::Parser;
use tracing::{error, info};

mod config;
mod decoder;
mod input;
mod renderer;
mod session;
mod stream;
#[cfg(feature = "x11-clipboard")]
mod clipboard_x11;

use config::ClientConfig;

#[derive(Parser, Debug)]
#[command(
    name    = "nidan-client",
    about   = "NIDAN — client de bureau distant sécurisé",
    version = env!("CARGO_PKG_VERSION"),
)]
struct Args {
    /// Adresse du broker NIDAN (ex: broker.interne.fr:7443)
    #[arg(short, long)]
    broker: Option<String>,

    /// Chemin vers le fichier de configuration
    #[arg(short, long, default_value = "/etc/nidan-client.toml")]
    config: String,

    /// Niveau de log
    #[arg(long)]
    log_level: Option<String>,

    /// Mode plein écran
    #[arg(long)]
    fullscreen: bool,

    /// Connexion directe au serveur (bypasse le broker, dev uniquement)
    #[arg(long)]
    direct: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(ref level) = args.log_level {
        std::env::set_var("NIDAN_LOG", level);
    }

    nidan_common::logging::init("nidan-client");

    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    info!(version = env!("CARGO_PKG_VERSION"), "nidan-client démarrage");

    let mut cfg = ClientConfig::load(&args.config)
        .unwrap_or_else(|_| {
            info!("config non trouvée — utilisation des valeurs par défaut");
            ClientConfig::default()
        });

    // Surcharges CLI
    if let Some(broker) = args.broker {
        cfg.network.broker_addr = broker;
    }
    if args.fullscreen {
        cfg.display.fullscreen = true;
    }
    if let Some(direct) = args.direct {
        cfg.network.direct_server = Some(direct);
    }

    info!(
        broker = %cfg.network.broker_addr,
        fullscreen = cfg.display.fullscreen,
        codec = %cfg.video.preferred_codec,
        "configuration chargée"
    );

    let client = stream::NidanClient::new(cfg).await
        .context("initialisation client NIDAN")?;

    match client.run().await {
        Ok(()) => info!("client arrêté proprement"),
        Err(e) => {
            error!(error = %e, "erreur fatale client");
            return Err(e);
        }
    }

    Ok(())
}
