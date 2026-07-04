//! Backend de capture v2 : `VsockCapturer`.
//!
//! Écoute côté hôte Proxmox sur un port vsock, accepte la connexion de
//! `nidan-agent` (dans une VM invitée), effectue le handshake miroir
//! du côté agent, puis lit les `AgentMessage::Frame` reçus et les convertit
//! en `capture::RawFrame` (v1) pour les passer à l'encodeur H.264 existant.
//!
//! Ce module est le pendant côté proxy du module `vsock_link` de
//! `nidan-agent`. C'est le pont concret qui réalise le modèle Sanzu :
//! la VM envoie des pixels bruts, le proxy (zone de confiance) encode.

use anyhow::{Context, Result};
use prost::Message;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tokio_vsock::{VsockAddr, VsockListener, VMADDR_CID_ANY};
use tracing::{debug, info, warn};

use super::{CapturerCapabilities, PixelFormat as V1PixelFormat, RawFrame};
use super::Capturer;

// Types générés depuis agent.proto par prost-build (côté nidan-proto v2).
use nidan_proto::agent::{
    agent_message, AgentMessage, PixelFormat as ProtoPixelFormat,
    ProxyHelloAck, StartCapture,
};

/// Capacités par défaut annoncées avant qu'un agent ne se connecte.
/// Elles seront affinées après réception du `AgentHello`, mais le proxy
/// doit annoncer quelque chose au démarrage pour que le stream serveur
/// puisse démarrer sa négociation avec le client.
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;

/// Port vsock par défaut sur lequel le VsockCapturer écoute côté hôte.
pub const DEFAULT_VSOCK_PORT: u32 = 6100;

/// Le VsockCapturer est instancié une fois, garde le port d'écoute et
/// des capacités par défaut. La vraie écoute + handshake se font dans
/// `start()` (côté implémentation du trait Capturer).
pub struct VsockCapturer {
    port: u32,
    caps: CapturerCapabilities,
    /// Canal d'entrées (proxy → agent). Le stream serveur envoie ici
    /// les `InputBatch` reçus du client, on les fera passer sur vsock.
    /// Rendu accessible via `inputs_tx()` pour que le code du stream
    /// puisse s'y abonner.
    inputs_tx: mpsc::Sender<Vec<u8>>,
    inputs_rx: Arc<Mutex<Option<mpsc::Receiver<Vec<u8>>>>>,
}

impl VsockCapturer {
    pub fn new(port: u32) -> Result<Arc<Self>> {
        let caps = CapturerCapabilities {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            supports_xshm: false,
            supports_xdamage: false,
            pixel_format: V1PixelFormat::Bgra8888,
        };
        let (inputs_tx, inputs_rx) = mpsc::channel::<Vec<u8>>(64);
        Ok(Arc::new(VsockCapturer {
            port,
            caps,
            inputs_tx,
            inputs_rx: Arc::new(Mutex::new(Some(inputs_rx))),
        }))
    }

    /// Handle pour envoyer des InputBatch sérialisés (protobuf) à l'agent.
    /// Le stream serveur récupère ce handle et lui pousse chaque batch reçu
    /// du client. La connexion vsock (établie dans start()) les enverra.
    pub fn inputs_tx(&self) -> mpsc::Sender<Vec<u8>> {
        self.inputs_tx.clone()
    }
}

impl Capturer for VsockCapturer {
    fn capabilities(&self) -> &CapturerCapabilities {
        &self.caps
    }

    fn start(
        self: Arc<Self>,
        tx: mpsc::Sender<RawFrame>,
        fps_limit: u32,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let port = self.port;
        let inputs_rx_arc = self.inputs_rx.clone();

        tokio::spawn(async move {
            info!(port, "VsockCapturer : écoute vsock côté hôte");

            // Écoute vsock. VMADDR_CID_ANY accepte les connexions de n'importe
            // quel CID (utile : ça marche que l'agent soit dans n'importe quelle
            // VM du pool, sans qu'on ait à durcir le CID côté proxy).
            let listen_addr = VsockAddr::new(VMADDR_CID_ANY, port);
            let mut listener = VsockListener::bind(listen_addr)
                .with_context(|| format!("bind vsock port {port}"))?;

            // Boucle d'accept : on n'accepte qu'une session à la fois (le proxy
            // est mono-session par VM cible, comme en v1).
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        info!("VsockCapturer : shutdown reçu");
                        break;
                    }
                    accept = listener.accept() => {
                        let (stream, peer) = match accept {
                            Ok(v) => v,
                            Err(e) => {
                                warn!(error = %e, "VsockCapturer : accept a échoué, on continue");
                                continue;
                            }
                        };
                        info!(
                            peer_cid = peer.cid(),
                            peer_port = peer.port(),
                            "VsockCapturer : agent connecté"
                        );

                        // Reprend le receiver d'inputs (déplacé dans la session).
                        // Si une session précédente l'a déjà consommé, on en crée
                        // un nouveau — cas où l'agent reconnecte après crash.
                        let inputs_rx = {
                            let mut guard = inputs_rx_arc.lock().await;
                            guard.take()
                        };

                        // Une session complète est gérée. Elle se termine soit
                        // sur EOF de l'agent, soit sur shutdown, soit sur erreur.
                        let result = run_session(
                            stream,
                            tx.clone(),
                            inputs_rx,
                            fps_limit,
                            shutdown.clone(),
                        )
                        .await;

                        if let Err(e) = result {
                            warn!(error = %e, "session vsock terminée avec erreur");
                        }
                        info!("VsockCapturer : session terminée, en attente d'une nouvelle connexion");
                    }
                }
            }
            Ok(())
        })
    }
}

/// Session unique proxy ↔ agent. Handshake, réception frames, envoi inputs.
async fn run_session(
    stream: tokio_vsock::VsockStream,
    frames_tx: mpsc::Sender<RawFrame>,
    inputs_rx: Option<mpsc::Receiver<Vec<u8>>>,
    fps_limit: u32,
    shutdown: CancellationToken,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);

    // 1. Recevoir AgentHello (première trame envoyée par l'agent).
    let hello_msg = recv_framed(&mut reader).await
        .context("réception AgentHello")?;
    let Some(agent_message::Msg::Hello(hello)) = hello_msg.msg else {
        anyhow::bail!("attendu AgentHello en première trame, reçu autre chose");
    };
    info!(
        agent_version = %hello.agent_version,
        native_width = hello.native_width,
        native_height = hello.native_height,
        max_fps = hello.max_fps,
        "AgentHello reçu"
    );

    // 2. Choisir un format supporté par l'agent, en préférant le premier annoncé
    // (par convention agent : premier = format natif du capturer local).
    let chosen_format = hello.supported_formats.first()
        .and_then(|f| ProtoPixelFormat::try_from(*f).ok())
        .unwrap_or(ProtoPixelFormat::Bgra8);
    info!(?chosen_format, "format choisi pour la session");

    // 3. Envoyer ProxyHelloAck.
    let ack = AgentMessage {
        msg: Some(agent_message::Msg::HelloAck(ProxyHelloAck {
            accepted: true,
            proxy_version: env!("CARGO_PKG_VERSION").to_string(),
            error_message: String::new(),
        })),
    };
    send_framed(&mut writer, &ack).await.context("envoi ProxyHelloAck")?;
    debug!("ProxyHelloAck envoyé");

    // 4. Envoyer StartCapture pour démarrer le flux.
    let fps = if fps_limit > 0 { fps_limit } else { hello.max_fps };
    let target_width = if hello.native_width > 0 { hello.native_width } else { DEFAULT_WIDTH };
    let target_height = if hello.native_height > 0 { hello.native_height } else { DEFAULT_HEIGHT };
    let start = AgentMessage {
        msg: Some(agent_message::Msg::Start(StartCapture {
            target_width,
            target_height,
            target_fps: fps,
            format: chosen_format as i32,
            correlation_id: format!("session-{}", std::process::id()),
        })),
    };
    send_framed(&mut writer, &start).await.context("envoi StartCapture")?;
    info!(target_width, target_height, target_fps = fps, "StartCapture envoyé — session active");

    // 5. Tâche parallèle : relayer les inputs (proxy → agent).
    let inputs_task = if let Some(mut rx) = inputs_rx {
        let mut writer_handle = writer;
        let shutdown_inputs = shutdown.clone();
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_inputs.cancelled() => break,
                    maybe_batch = rx.recv() => {
                        let Some(batch_bytes) = maybe_batch else { break; };
                        // Envelopper le blob dans AgentMessage::Inputs et l'envoyer
                        // sur le canal vsock. L'agent le décodera comme InputBatch v1
                        // (JSON) et l'injectera via RemoteDesktop.
                        let msg = AgentMessage {
                            msg: Some(agent_message::Msg::Inputs(batch_bytes)),
                        };
                        if let Err(e) = send_framed(&mut writer_handle, &msg).await {
                            warn!(error = %e, "erreur envoi InputBatch sur vsock — arrêt du relais");
                            break;
                        }
                        debug!("InputBatch relayé sur vsock");
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        }))
    } else {
        writer.shutdown().await.ok();
        None
    };

    // 6. Boucle principale : recevoir les frames et les passer à l'encodeur.
    let mut frames_recv: u64 = 0;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("VsockCapturer : shutdown pendant la session");
                break;
            }
            res = recv_framed(&mut reader) => {
                let msg = match res {
                    Ok(m) => m,
                    Err(e) => {
                        info!(error = %e, "fin de session vsock (peer close ou erreur)");
                        break;
                    }
                };
                match msg.msg {
                    Some(agent_message::Msg::Frame(proto_frame)) => {
                        // Conversion : ProtoRawFrame v2 → RawFrame v1 (attendue par l'encodeur).
                        let raw = RawFrame {
                            data:         proto_frame.pixels,
                            width:        proto_frame.width,
                            height:       proto_frame.height,
                            stride:       proto_frame.stride_bytes,
                            timestamp_us: proto_frame.timestamp_us,
                            seq:          proto_frame.frame_seq,
                            is_keyframe:  true, // pas d'info différentielle sur vsock pour l'instant
                            damage_rects: vec![],
                        };
                        if frames_tx.send(raw).await.is_err() {
                            warn!("channel encodeur fermé — arrêt de la session vsock");
                            break;
                        }
                        frames_recv += 1;
                        if frames_recv % 30 == 0 {
                            debug!(frames_recv, "vsock : frames reçues");
                        }
                    }
                    Some(agent_message::Msg::Stopped(stop)) => {
                        info!(reason = ?stop.reason, detail = %stop.detail, "agent a envoyé AgentStopped");
                        break;
                    }
                    Some(other) => {
                        debug!(?other, "message vsock ignoré (pas une frame)");
                    }
                    None => {
                        debug!("message vsock vide (oneof None)");
                    }
                }
            }
        }
    }

    // 7. Nettoyage.
    if let Some(handle) = inputs_task {
        handle.abort();
        let _ = handle.await;
    }
    info!(frames_recv, "session vsock terminée proprement");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Framing [u32 length LE][protobuf bytes] — identique au module vsock_link
// de l'agent, pour rester symétrique.
// ─────────────────────────────────────────────────────────────────────────────

async fn send_framed<W: AsyncWriteExt + Unpin>(writer: &mut W, msg: &AgentMessage) -> Result<()> {
    let payload = msg.encode_to_vec();
    let len = (payload.len() as u32).to_le_bytes();
    writer.write_all(&len).await.context("écriture longueur")?;
    writer.write_all(&payload).await.context("écriture payload")?;
    Ok(())
}

async fn recv_framed<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<AgentMessage> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.context("lecture longueur")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 32 * 1024 * 1024 {
        anyhow::bail!("message vsock trop grand : {} octets", len);
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await.context("lecture payload")?;
    let msg = AgentMessage::decode(&payload[..]).context("décodage protobuf")?;
    Ok(msg)
}
