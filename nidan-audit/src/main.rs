//! # nidan-audit (daemon)
//!
//! Daemon d'audit NIDAN. Reçoit les événements des autres composants
//! (server, broker) et assure : recording MKV, watermarking, métriques.

#![forbid(unsafe_code)]

use anyhow::Context;
use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "nidan-audit", about = "NIDAN — daemon d'audit forensique", version)]
struct Args {
    #[arg(short, long, default_value = "/etc/nidan-audit.toml")]
    config: String,
    #[arg(long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(ref l) = args.log_level {
        std::env::set_var("NIDAN_LOG", l);
    }
    nidan_common::logging::init("nidan-audit");
    info!(version = env!("CARGO_PKG_VERSION"), "nidan-audit démarrage");

    let cfg = nidan_audit::config::AuditDaemonConfig::load(&args.config)
        .unwrap_or_default();

    let engine = nidan_audit::AuditEngine::new(cfg).await
        .context("initialisation moteur audit")?;

    engine.run().await
}
