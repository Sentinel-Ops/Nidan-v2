//! # nidan-host-agent
//!
//! Agent de pilotage libvirt pour le socle NIDAN.
//! Écoute sur vsock et relaie les requêtes du broker vers libvirtd.

#![forbid(unsafe_code)]

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_vsock::{VsockListener, VMADDR_CID_ANY};
use tracing::{error, info, warn};

mod config;
mod handler;
mod libvirt_ops;

use config::HostAgentConfig;

#[derive(Parser, Debug)]
#[command(
    name = "nidan-host-agent",
    about = "NIDAN — agent de pilotage libvirt (vsock)",
    version
)]
struct Args {
    /// Chemin du fichier de configuration
    #[arg(short, long, default_value = "/etc/nidan/host-agent.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    nidan_common::logging::init("nidan-host-agent");
    info!(version = env!("CARGO_PKG_VERSION"), "démarrage");

    let cfg = HostAgentConfig::load(&args.config)?;
    cfg.validate()?;

    // Vérifier la connectivité libvirt au démarrage
    libvirt_ops::check_connectivity(&cfg.libvirt.uri)
        .context("connexion libvirt initiale")?;
    info!(uri = %cfg.libvirt.uri, "libvirt connecté");

    let cfg = Arc::new(cfg);

    // Écoute vsock
    let mut listener = VsockListener::bind(tokio_vsock::VsockAddr::new(VMADDR_CID_ANY, cfg.vsock.port))
        .with_context(|| format!("bind vsock port {}", cfg.vsock.port))?;

    info!(port = cfg.vsock.port, "écoute vsock démarrée");

    loop {
        let (stream, addr) = listener.accept().await
            .context("accept vsock")?;

        let peer_cid = addr.cid();

        // Vérifier le CID source
        if let Some(allowed) = cfg.vsock.allowed_cid {
            if peer_cid != allowed {
                warn!(
                    peer_cid = peer_cid,
                    allowed = allowed,
                    "connexion refusée : CID non autorisé"
                );
                continue;
            }
        }

        let cfg = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peer_cid, &cfg).await {
                error!(
                    peer_cid = peer_cid,
                    error = %e,
                    "erreur traitement requête"
                );
            }
        });
    }
}

/// Traite une connexion vsock : lit la requête, dispatch, écrit la réponse.
async fn handle_connection(
    mut stream: tokio_vsock::VsockStream,
    peer_cid: u32,
    cfg: &HostAgentConfig,
) -> Result<()> {
    // Lire la longueur (4 bytes BE)
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await
        .context("lecture longueur")?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > 64 * 1024 {
        anyhow::bail!("requête trop grande: {len} bytes");
    }

    // Lire le payload JSON
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await
        .context("lecture payload")?;

    // Désérialiser
    let request: nidan_proto::host_agent::AgentRequest =
        serde_json::from_slice(&buf)
            .context("désérialisation requête")?;

    info!(
        peer_cid = peer_cid,
        action = ?std::mem::discriminant(&request),
        "requête reçue"
    );

    // Dispatch
    let response = handler::handle_request(request, cfg).await;

    // Sérialiser et envoyer la réponse
    let resp_bytes = serde_json::to_vec(&response)
        .context("sérialisation réponse")?;
    let resp_len = (resp_bytes.len() as u32).to_be_bytes();

    stream.write_all(&resp_len).await?;
    stream.write_all(&resp_bytes).await?;
    drop(stream);

    Ok(())
}
