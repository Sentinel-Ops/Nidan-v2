//! # nidan-broker
//!
//! Point d'entrée unique de l'architecture NIDAN.
//! Gère l'authentification, le pool de VMs et le routing QUIC.
//!
//! ## Services exposés
//! - Port 7443 UDP : endpoint QUIC (clients)
//! - Port 7080 TCP : API REST admin (interne uniquement)

#![forbid(unsafe_code)]

use anyhow::Context;
use clap::Parser;
use tracing::info;

mod api;
mod auth;
mod config;
mod pool;
mod routing;
mod session;

use config::BrokerConfig;

#[derive(Parser, Debug)]
#[command(name = "nidan-broker", about = "NIDAN — broker d'accès sécurisé", version)]
struct Args {
    #[arg(short, long, default_value = "/etc/nidan-broker.toml")]
    config: String,
    #[arg(long)]
    log_level: Option<String>,
    #[arg(long)]
    print_config: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(ref l) = args.log_level {
        std::env::set_var("NIDAN_LOG", l);
    }
    nidan_common::logging::init("nidan-broker");
    info!(version = env!("CARGO_PKG_VERSION"), "nidan-broker démarrage");

    let cfg = BrokerConfig::load(&args.config)
        .unwrap_or_else(|_| {
            info!("config non trouvée — valeurs par défaut");
            BrokerConfig::default()
        });

    if args.print_config {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }

    cfg.validate().context("configuration invalide")?;

    // State partagé entre tous les composants
    let state = routing::BrokerState::new(cfg.clone()).await
        .context("initialisation état broker")?;

    // Démarrage des services en parallèle
    tokio::try_join!(
        routing::run_quic_server(state.clone()),
        api::run_admin_api(state.clone()),
    )?;

    Ok(())
}
