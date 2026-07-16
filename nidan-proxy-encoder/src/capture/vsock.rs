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
    /// Étape 6i-bis : le receiver du canal inputs vit pour toute la durée du
    /// process, partagé sous Mutex entre les sessions vsock successives.
    ///
    /// Ancien design : `Arc<Mutex<Option<Receiver>>>` avec `guard.take()` à
    /// chaque session. Bug : après la première session, le `Option` restait
    /// `None` définitivement (aucun code ne le repopulait, malgré le commentaire
    /// qui décrivait l'intention). Toutes les sessions suivantes tombaient
    /// dans la branche `else` de `run_session` qui faisait un `writer.shutdown()`.
    /// Ce half-close côté proxy provoquait un EOF sur le reader de l'agent
    /// (WARN "erreur lecture vsock — lecture longueur"), ce qui tuait sa
    /// reader_task et empêchait toute injection d'input (fenêtre affichée
    /// mais aucune interaction possible).
    ///
    /// Nouveau design : le receiver n'est jamais `take()`é, on le prête via un
    /// `MutexGuard` détenu par la session en cours. Comme le VsockCapturer est
    /// mono-session par VM (une seule connexion agent active à la fois), la
    /// contention sur ce Mutex est nulle en pratique.
    inputs_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    /// Etape 6c : etat partage des capabilities, rempli a AgentHello.
    shared_caps: Option<Arc<tokio::sync::RwLock<Option<CapturerCapabilities>>>>,
    /// Etape 6c : notifieur reveille quand shared_caps est mis a jour.
    caps_notify: Option<Arc<tokio::sync::Notify>>,
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
            inputs_rx: Arc::new(Mutex::new(inputs_rx)),
            shared_caps: None,
            caps_notify: None,
        }))
    }

    /// Etape 6c : constructeur avec caps partagees, mis a jour a AgentHello.
    pub fn new_with_shared_caps(
        port: u32,
        shared_caps: Arc<tokio::sync::RwLock<Option<CapturerCapabilities>>>,
        caps_notify: Arc<tokio::sync::Notify>,
    ) -> Result<Arc<Self>> {
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
            inputs_rx: Arc::new(Mutex::new(inputs_rx)),
            shared_caps: Some(shared_caps),
            caps_notify: Some(caps_notify),
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

                        // Étape 6i-bis : on ne consomme plus le receiver, on le
                        // prête à la session via un clone de l'Arc<Mutex>. La
                        // session lock le Mutex pour toute sa durée, puis le
                        // libère au drop — la prochaine session peut à nouveau
                        // relayer les inputs sans qu'on ait à recréer le canal
                        // (ce qui aurait invalidé les `inputs_tx` déjà distribués
                        // au VsockService).
                        let inputs_rx = Arc::clone(&inputs_rx_arc);

                        // Une session complète est gérée. Elle se termine soit
                        // sur EOF de l'agent, soit sur shutdown, soit sur erreur.
                        let result = run_session(
                            stream,
                            tx.clone(),
                            inputs_rx,
                            fps_limit,
                            shutdown.clone(),
                            self.shared_caps.clone(),
                            self.caps_notify.clone(),
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
    inputs_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    fps_limit: u32,
    shutdown: CancellationToken,
    shared_caps_opt: Option<Arc<tokio::sync::RwLock<Option<CapturerCapabilities>>>>,
    caps_notify_opt: Option<Arc<tokio::sync::Notify>>,
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

    // Etape 6c : mise a jour des capabilities partagees avec les vraies dimensions.
    if let (Some(shared), Some(notify)) = (&shared_caps_opt, &caps_notify_opt) {
        let real_caps = CapturerCapabilities {
            width: if hello.native_width > 0 { hello.native_width } else { DEFAULT_WIDTH },
            height: if hello.native_height > 0 { hello.native_height } else { DEFAULT_HEIGHT },
            supports_xshm: false,
            supports_xdamage: false,
            pixel_format: V1PixelFormat::Bgra8888,
        };
        let mut guard = shared.write().await;
        *guard = Some(real_caps.clone());
        drop(guard);
        notify.notify_waiters();
        info!(
            width = real_caps.width,
            height = real_caps.height,
            "capabilities partagees mises a jour"
        );
    }

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
    //
    // Étape 6i-bis : la branche `else { writer.shutdown() }` de l'ancienne
    // version a été supprimée — ce half-close côté proxy fermait le reader de
    // l'agent (EOF sur read_exact(len)), tuait sa reader_task et empêchait
    // toute injection d'input pour les sessions 2+. Le receiver est maintenant
    // toujours disponible via un Arc<Mutex<Receiver>> partagé (voir doc du
    // champ VsockCapturer::inputs_rx), donc cette branche n'a plus lieu d'être.
    let inputs_task = {
        let inputs_rx = Arc::clone(&inputs_rx);
        let shutdown_inputs = shutdown.clone();
        Some(tokio::spawn(async move {
            run_inputs_relay(writer, inputs_rx, shutdown_inputs).await
        }))
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
                        // Étape 6i : BUG TROUVÉ — avant, is_keyframe était codé en
                        // dur à `true` pour TOUTES les frames ("pas d'info
                        // différentielle sur vsock pour l'instant"). Ça forçait
                        // l'encodeur à produire un IDR complet sur CHAQUE frame
                        // (voir encoder/mod.rs : `if raw_frame.is_keyframe {
                        // encoder.request_keyframe() }`, exécuté à chaque appel).
                        // Un flux 100% IDR (jamais de P-frame) est un usage très
                        // atypique de H.264 — la doc du crate openh264 indique
                        // explicitement qu'un encodeur au comportement "exotique"
                        // peut produire des flux que leur décodeur ne gère pas
                        // proprement. C'est cohérent avec le décodage anormalement
                        // lent observé (120-150ms/frame, typique d'un flux
                        // uniquement en intra) et le blocage du décodeur après
                        // quelques dizaines de secondes.
                        // Fix : seule la toute première frame de la session est
                        // marquée keyframe. L'encodeur gère ensuite normalement
                        // son cycle périodique (keyframe_interval, ~toutes les
                        // 20 frames par défaut), produisant un flux IDR+P normal.
                        let is_keyframe = frames_recv == 0;
                        let raw = RawFrame {
                            data:         proto_frame.pixels,
                            width:        proto_frame.width,
                            height:       proto_frame.height,
                            stride:       proto_frame.stride_bytes,
                            timestamp_us: proto_frame.timestamp_us,
                            seq:          proto_frame.frame_seq,
                            is_keyframe,
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

// ─────────────────────────────────────────────────────────────────────────────
// Relais des inputs proxy → agent, factorisé pour être testable indépendamment
// du transport vsock (fonction générique sur AsyncWrite).
// ─────────────────────────────────────────────────────────────────────────────

/// Consomme les `InputBatch` (JSON) depuis `inputs_rx`, les enveloppe dans
/// un `AgentMessage::Inputs` framed, et les écrit sur `writer` jusqu'à ce que :
///   • `shutdown` soit cancellé,
///   • ou le canal source soit fermé (aucun sender restant, cas de test),
///   • ou une erreur d'écriture survienne.
///
/// La signature est volontairement générique pour permettre les tests avec
/// `tokio::io::duplex` sans dépendre du kernel vsock.
async fn run_inputs_relay<W: AsyncWriteExt + Unpin>(
    mut writer: W,
    inputs_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    shutdown: CancellationToken,
) -> Result<()> {
    // On garde le MutexGuard pendant toute la durée de la session : c'est
    // ce qui empêche deux sessions concurrentes de se voler mutuellement les
    // batches. Comme le VsockCapturer est mono-session par VM, la contention
    // est nulle : le lock est libre entre les sessions.
    let mut rx_guard = inputs_rx.lock().await;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            maybe_batch = rx_guard.recv() => {
                let Some(batch_bytes) = maybe_batch else { break; };
                // Envelopper le blob dans AgentMessage::Inputs et l'envoyer
                // sur le canal vsock. L'agent le décodera comme InputBatch v1
                // (JSON) et l'injectera via RemoteDesktop.
                let msg = AgentMessage {
                    msg: Some(agent_message::Msg::Inputs(batch_bytes)),
                };
                if let Err(e) = send_framed(&mut writer, &msg).await {
                    warn!(error = %e, "erreur envoi InputBatch sur vsock — arrêt du relais");
                    break;
                }
                debug!("InputBatch relayé sur vsock");
            }
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests unitaires — non-régression du bug "sessions 2+ sans inputs".
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt};

    /// Aide test : lit une AgentMessage framed depuis un reader `duplex`.
    /// Duplique volontairement la logique de `recv_framed` (le vrai
    /// `recv_framed` est déjà testé indirectement par les tests d'intégration
    /// du crate) pour rester indépendant de sa signature.
    async fn read_one_framed<R: AsyncReadExt + Unpin>(reader: &mut R) -> AgentMessage {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await.expect("lecture longueur");
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await.expect("lecture payload");
        AgentMessage::decode(&buf[..]).expect("décodage protobuf")
    }

    fn extract_input_bytes(msg: AgentMessage) -> Vec<u8> {
        match msg.msg {
            Some(agent_message::Msg::Inputs(bytes)) => bytes,
            other => panic!("attendu AgentMessage::Inputs, reçu {:?}", other),
        }
    }

    /// Régression : dans l'ancien design, la 2e session ne recevait plus
    /// aucun input parce que `guard.take()` avait consommé le `Option<Receiver>`
    /// à la 1re session et que rien ne le repopulait. Ce test simule deux
    /// sessions consécutives sur le même `Arc<Mutex<Receiver>>` et vérifie
    /// que chaque session voit bien passer son batch.
    #[tokio::test]
    async fn inputs_relay_survives_multiple_sessions() {
        let (inputs_tx, inputs_rx) = mpsc::channel::<Vec<u8>>(8);
        let inputs_rx = Arc::new(Mutex::new(inputs_rx));

        // ── Session 1 ──────────────────────────────────────────────────
        let (client_end_1, mut agent_end_1) = duplex(4096);
        let shutdown_1 = CancellationToken::new();
        let relay_1 = tokio::spawn(run_inputs_relay(
            client_end_1,
            Arc::clone(&inputs_rx),
            shutdown_1.clone(),
        ));

        inputs_tx.send(b"batch-session-1".to_vec()).await.unwrap();
        let received = read_one_framed(&mut agent_end_1).await;
        assert_eq!(
            extract_input_bytes(received),
            b"batch-session-1",
            "session 1 doit recevoir son batch"
        );

        // Fin de session 1 : on cancel, la task quitte, le MutexGuard est droppé.
        shutdown_1.cancel();
        relay_1.await.unwrap().expect("relay 1 doit se terminer proprement");

        // ── Session 2 ──────────────────────────────────────────────────
        // C'est ici que l'ancien code cassait : le receiver était `None`,
        // `run_session` faisait `writer.shutdown()`, l'agent voyait EOF.
        let (client_end_2, mut agent_end_2) = duplex(4096);
        let shutdown_2 = CancellationToken::new();
        let relay_2 = tokio::spawn(run_inputs_relay(
            client_end_2,
            Arc::clone(&inputs_rx),
            shutdown_2.clone(),
        ));

        inputs_tx.send(b"batch-session-2".to_vec()).await.unwrap();
        let received = read_one_framed(&mut agent_end_2).await;
        assert_eq!(
            extract_input_bytes(received),
            b"batch-session-2",
            "session 2 doit AUSSI recevoir son batch (régression du bug guard.take())"
        );

        shutdown_2.cancel();
        relay_2.await.unwrap().expect("relay 2 doit se terminer proprement");
    }

    /// Vérifie qu'un `shutdown.cancel()` interrompt bien le relais et que
    /// la task se termine sans erreur (utile pour garantir que la fin de
    /// session libère le MutexGuard sans panique).
    #[tokio::test]
    async fn inputs_relay_terminates_on_shutdown() {
        let (_inputs_tx, inputs_rx) = mpsc::channel::<Vec<u8>>(1);
        let inputs_rx = Arc::new(Mutex::new(inputs_rx));

        let (client_end, _agent_end) = duplex(1024);
        let shutdown = CancellationToken::new();

        let handle = tokio::spawn(run_inputs_relay(
            client_end,
            Arc::clone(&inputs_rx),
            shutdown.clone(),
        ));

        // Rien envoyé sur inputs_tx : la task doit rester bloquée dans le
        // select! jusqu'au cancel.
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("la task doit se terminer après cancel (pas de deadlock)")
            .unwrap()
            .expect("run_inputs_relay doit retourner Ok");
    }

    /// Vérifie que si un batch arrive pendant la session, il est relayé
    /// tel quel (pas de troncature, pas de corruption du framing).
    #[tokio::test]
    async fn inputs_relay_preserves_batch_bytes() {
        let (inputs_tx, inputs_rx) = mpsc::channel::<Vec<u8>>(4);
        let inputs_rx = Arc::new(Mutex::new(inputs_rx));

        let (client_end, mut agent_end) = duplex(8192);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(run_inputs_relay(
            client_end,
            Arc::clone(&inputs_rx),
            shutdown.clone(),
        ));

        // Un batch avec un contenu qui contient tous les octets 0..=255,
        // pour détecter toute corruption d'encodage/framing.
        let payload: Vec<u8> = (0u8..=255).collect();
        inputs_tx.send(payload.clone()).await.unwrap();

        let received = read_one_framed(&mut agent_end).await;
        assert_eq!(extract_input_bytes(received), payload);

        shutdown.cancel();
        handle.await.unwrap().unwrap();
    }
}
