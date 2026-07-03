//! Gestion du canal vsock entre l'agent (VM) et le proxy-encoder (hôte).
//!
//! Ce module est le vrai code nouveau de l'étape 4. Il implémente :
//!
//!   • Le handshake initial : `AgentHello` → `ProxyHelloAck`.
//!   • Le framing sur vsock : `[u32 length LE][protobuf bytes]`, comme
//!     défini dans nidan-proto/proto/agent.proto.
//!   • L'envoi de `RawFrame` (pixels bruts) depuis le capturer local.
//!   • La réception de `InputBatch` (entrées clavier/souris) depuis le
//!     proxy, à relayer vers l'injection RemoteDesktop.
//!   • La gestion propre des cas d'erreur : arrêt du portail, coupure
//!     de vsock, StopCapture reçu.
//!
//! Note : la sérialisation utilise `prost` (les types Rust générés
//! depuis agent.proto par prost-build). Le framing est ajouté à la
//! main par-dessus, parce que prost ne fait pas de délimitation.

use anyhow::{Context, Result};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_vsock::{VsockAddr, VsockStream};
use tracing::{debug, info, warn};

// Types générés depuis agent.proto par prost-build.
use nidan_proto::agent::{
    agent_message, AgentHello, AgentMessage, AgentStopReason, AgentStopped,
    PixelFormat, ProxyHelloAck, RawFrame as ProtoRawFrame, StartCapture,
    StopCapture,
};

/// Buffer de frames en attente d'envoi vers l'hôte.
/// Taille bornée pour éviter de saturer la RAM si le proxy ralentit.
const FRAME_QUEUE_SIZE: usize = 4;

/// Ordre reçu depuis le proxy à propager au reste de l'agent.
pub enum ProxyCommand {
    Start(StartCapture),
    Stop(StopCapture),
    Inputs(Vec<u8>), // InputBatch sérialisé (à parser côté injecteur)
}

/// Établit la connexion vsock avec le proxy et effectue le handshake.
///
/// Retourne le stream ouvert et la config de capture demandée par le proxy.
pub async fn connect_and_handshake(
    host_cid: u32,
    port: u32,
    supported_formats: Vec<PixelFormat>,
    native_width: u32,
    native_height: u32,
    max_fps: u32,
) -> Result<(VsockStream, StartCapture)> {
    info!(host_cid, port, "connexion vsock vers le proxy-encoder");
    let addr = VsockAddr::new(host_cid, port);
    let mut stream = VsockStream::connect(addr)
        .await
        .with_context(|| format!("connexion vsock cid={host_cid} port={port}"))?;
    info!("connecté au proxy");

    // 1. Envoi de AgentHello.
    let hello = AgentHello {
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        supported_formats: supported_formats.iter().map(|f| *f as i32).collect(),
        native_width,
        native_height,
        max_fps,
    };
    let msg = AgentMessage {
        msg: Some(agent_message::Msg::Hello(hello)),
    };
    send_framed(&mut stream, &msg).await
        .context("envoi AgentHello")?;
    debug!("AgentHello envoyé");

    // 2. Réception ProxyHelloAck.
    let received = recv_framed(&mut stream).await
        .context("réception ProxyHelloAck")?;
    let Some(agent_message::Msg::HelloAck(ack)) = received.msg else {
        anyhow::bail!("attendu ProxyHelloAck, reçu autre chose");
    };
    if !ack.accepted {
        anyhow::bail!("proxy a refusé la session : {}", ack.error_message);
    }
    info!(proxy_version = %ack.proxy_version, "handshake vsock OK");

    // 3. Réception StartCapture (le proxy pilote le démarrage).
    let start_msg = recv_framed(&mut stream).await
        .context("réception StartCapture")?;
    let Some(agent_message::Msg::Start(start)) = start_msg.msg else {
        anyhow::bail!("attendu StartCapture après handshake");
    };
    info!(
        width = start.target_width,
        height = start.target_height,
        fps = start.target_fps,
        correlation_id = %start.correlation_id,
        "StartCapture reçu du proxy — démarrage de la capture"
    );

    Ok((stream, start))
}

/// Boucle principale une fois le handshake fait : lit les frames du
/// capturer local, les envoie sur vsock. En parallèle, lit les messages
/// entrants du proxy (Stop, InputBatch) et les propage.
pub async fn run_session(
    mut stream: VsockStream,
    mut frames_rx: mpsc::Receiver<crate::capture::RawFrame>,
    cmd_tx: mpsc::Sender<ProxyCommand>,
    format: PixelFormat,
    shutdown: CancellationToken,
) -> Result<()> {
    // On split le stream en (reader, writer) pour lire et écrire en parallèle.
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Tâche 1 : lire les messages entrants du proxy et les propager.
    let cmd_tx_task = cmd_tx.clone();
    let shutdown_reader = shutdown.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_reader.cancelled() => {
                    debug!("reader vsock : shutdown reçu");
                    break;
                }
                res = recv_framed_reader(&mut reader) => {
                    match res {
                        Ok(msg) => {
                            if let Some(inner) = msg.msg {
                                match inner {
                                    agent_message::Msg::Stop(stop) => {
                                        info!(correlation_id = %stop.correlation_id, "StopCapture reçu du proxy");
                                        let _ = cmd_tx_task.send(ProxyCommand::Stop(stop)).await;
                                        break;
                                    }
                                    agent_message::Msg::Start(start) => {
                                        // Cas rare : re-Start (reconfiguration en cours de session).
                                        info!("StartCapture reçu en cours de session (reconfiguration)");
                                        let _ = cmd_tx_task.send(ProxyCommand::Start(start)).await;
                                    }
                                    _ => {
                                        // Les InputBatch seraient là si on les propage via ce canal.
                                        // Pour l'étape 4, on ne les traite pas encore côté agent.
                                        debug!("message vsock ignoré (pas encore géré par l'agent)");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "erreur lecture vsock — fermeture de la boucle reader");
                            break;
                        }
                    }
                }
            }
        }
        debug!("reader vsock terminé");
    });

    // Tâche 2 (boucle courante) : envoyer les frames.
    let mut frames_sent: u64 = 0;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("shutdown reçu — arrêt de la boucle d'envoi");
                break;
            }
            maybe_frame = frames_rx.recv() => {
                let Some(frame) = maybe_frame else {
                    info!("le capturer a fermé son canal — fin de session");
                    break;
                };

                // Conversion : RawFrame v1 → ProtoRawFrame (v2 protobuf).
                let proto_frame = ProtoRawFrame {
                    frame_seq:    frame.seq,
                    timestamp_us: frame.timestamp_us,
                    width:        frame.width,
                    height:       frame.height,
                    format:       format as i32,
                    stride_bytes: frame.stride,
                    pixels:       frame.data,
                };
                let msg = AgentMessage {
                    msg: Some(agent_message::Msg::Frame(proto_frame)),
                };

                if let Err(e) = send_framed_writer(&mut writer, &msg).await {
                    warn!(error = %e, "erreur envoi RawFrame — arrêt de la session");
                    break;
                }
                frames_sent += 1;
                if frames_sent % 30 == 0 {
                    debug!(frames_sent, "envoi vsock : progression");
                }
            }
        }
    }

    // Notifier proprement le proxy qu'on s'arrête.
    let stopped = AgentStopped {
        reason: AgentStopReason::Shutdown as i32,
        detail: "agent local shutdown".to_string(),
        correlation_id: String::new(),
    };
    let final_msg = AgentMessage {
        msg: Some(agent_message::Msg::Stopped(stopped)),
    };
    let _ = send_framed_writer(&mut writer, &final_msg).await;
    info!(frames_sent, "session vsock terminée proprement");

    shutdown.cancel();
    let _ = reader_task.await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// FRAMING [u32 length LE][protobuf bytes]
// ─────────────────────────────────────────────────────────────────────────────

async fn send_framed(
    stream: &mut VsockStream,
    msg: &AgentMessage,
) -> Result<()> {
    let payload = msg.encode_to_vec();
    let len = (payload.len() as u32).to_le_bytes();
    stream.write_all(&len).await.context("écriture longueur")?;
    stream.write_all(&payload).await.context("écriture payload")?;
    Ok(())
}

async fn send_framed_writer<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &AgentMessage,
) -> Result<()> {
    let payload = msg.encode_to_vec();
    let len = (payload.len() as u32).to_le_bytes();
    writer.write_all(&len).await.context("écriture longueur")?;
    writer.write_all(&payload).await.context("écriture payload")?;
    Ok(())
}

async fn recv_framed(stream: &mut VsockStream) -> Result<AgentMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.context("lecture longueur")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 32 * 1024 * 1024 {
        anyhow::bail!("message vsock trop grand : {} octets", len);
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.context("lecture payload")?;
    let msg = AgentMessage::decode(&payload[..]).context("décodage protobuf")?;
    Ok(msg)
}

async fn recv_framed_reader<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<AgentMessage> {
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

// Taille par défaut du canal frames (importable par main.rs si besoin).
pub const FRAME_QUEUE_SIZE_DEFAULT: usize = FRAME_QUEUE_SIZE;
