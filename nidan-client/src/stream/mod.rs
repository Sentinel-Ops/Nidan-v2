//! Client QUIC NIDAN — connexion, handshake et orchestration du pipeline.
//!
//! ## Flux de connexion
//! ```text
//! NidanClient::new()
//!        ↓
//! [Connexion QUIC au broker (mTLS)]
//!        ↓
//! [ClientSessionRequest → BrokerSessionResponse]
//!        ↓
//! [Connexion QUIC au serveur assigné (token)]
//!        ↓
//! [ClientServerHandshake → ServerHandshakeAck]
//!        ↓
//! [Démarrage pipeline: stream QUIC → décodeur → renderer]
//!        ↓  (parallèle)
//! [renderer SDL2 → inputs → stream QUIC contrôle]
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use nidan_common::session::SessionId;
use nidan_proto::{
    BrokerSessionResponse, ClientServerHandshake, ClientSessionRequest,
    InputBatch, ServerHandshakeAck, VideoFrame,
};

use crate::config::ClientConfig;
use crate::decoder::{DecoderPipeline, DecodedFrame};
use crate::input::{InputEvent, InputSender};
use crate::renderer;
use crate::session::ClientSession;

const VIDEO_CHANNEL_SIZE:  usize = 8;
const DECODED_CHANNEL_SIZE: usize = 4;
const INPUT_CHANNEL_SIZE:   usize = 256;

/// Client NIDAN principal
pub struct NidanClient {
    config:   ClientConfig,
    endpoint: quinn::Endpoint,
}

impl NidanClient {
    /// Crée le client et initialise l'endpoint QUIC local
    pub async fn new(config: ClientConfig) -> Result<Self> {
        let tls_config = Self::build_tls_config(&config)
            .context("configuration TLS client")?;

        // Étape 6d : même config que côté proxy — voir le commentaire dans
        // nidan-proxy-encoder/src/stream/mod.rs pour le détail du mécanisme.
        let mut transport_config = quinn::TransportConfig::default();
        transport_config
            .max_idle_timeout(Some(std::time::Duration::from_secs(60).try_into()?))
            .keep_alive_interval(Some(std::time::Duration::from_secs(5)));

        let mut client_config = quinn::ClientConfig::new(Arc::new(tls_config));
        client_config.transport_config(Arc::new(transport_config));

        // Bind local sur port aléatoire
        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
            .context("création endpoint QUIC")?;
        endpoint.set_default_client_config(client_config);

        info!("endpoint QUIC client initialisé");
        Ok(Self { config, endpoint })
    }

    /// Construit la config TLS client (mTLS)
    fn build_tls_config(config: &ClientConfig) -> Result<quinn::crypto::rustls::QuicClientConfig> {
        use std::fs;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use rustls_pemfile::{certs as parse_certs, pkcs8_private_keys};

        let cert_pem = fs::read(&config.tls.cert).with_context(|| format!("cert: {}", config.tls.cert))?;
        let client_certs: Vec<CertificateDer> = parse_certs(&mut cert_pem.as_slice())
            .collect::<Result<Vec<_>, _>>()?;

        let key_pem = fs::read(&config.tls.key).with_context(|| format!("key: {}", config.tls.key))?;
        let mut keys = pkcs8_private_keys(&mut key_pem.as_slice())
            .collect::<Result<Vec<_>, _>>()?;
        if keys.is_empty() { anyhow::bail!("aucune clé PKCS8 dans {}", config.tls.key); }

        let ca_pem = fs::read(&config.tls.ca_cert).with_context(|| format!("ca: {}", config.tls.ca_cert))?;
        let ca_certs: Vec<CertificateDer> = parse_certs(&mut ca_pem.as_slice())
            .collect::<Result<Vec<_>, _>>()?;

        let mut root_store = rustls::RootCertStore::empty();
        for ca in ca_certs { root_store.add(ca)?; }

        let rustls_cfg = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(
                client_certs,
                PrivateKeyDer::Pkcs8(keys.remove(0))
            )?;

        quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
            .context("QuicClientConfig")
    }

    /// Boucle principale du client avec reconnexion automatique
    pub async fn run(self) -> Result<()> {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("Ctrl+C reçu — arrêt client");
            shutdown_clone.cancel();
        });

        let mut reconnect_count = 0u32;

        loop {
            if shutdown.is_cancelled() { break; }

            match self.run_session(shutdown.clone()).await {
                Ok(()) => {
                    info!("session terminée proprement");
                    break;
                }
                Err(e) => {
                    if shutdown.is_cancelled() { break; }

                    if !self.config.network.auto_reconnect {
                        return Err(e);
                    }

                    reconnect_count += 1;
                    let delay = self.config.network.reconnect_delay_secs;
                    warn!(
                        error        = %e,
                        attempt      = reconnect_count,
                        retry_in_sec = delay,
                        "erreur session — reconnexion"
                    );
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }

        Ok(())
    }

    /// Exécute une session complète (connexion → streaming → déconnexion)
    async fn run_session(
        &self,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        // ── Connexion ──────────────────────────────────────────────────────
        let (server_addr, session_token) = if let Some(ref direct) = self.config.network.direct_server {
            // Mode direct (dev) : bypass broker, pas de token
            info!(addr = %direct, "connexion directe au serveur (mode dev)");
            (direct.clone(), Vec::new())
        } else {
            // Mode normal : broker → session token → adresse serveur
            self.connect_via_broker().await
                .context("connexion via broker")?
        };

        let server_addr: SocketAddr = server_addr.parse()
            .with_context(|| format!("adresse serveur invalide: {server_addr}"))?;

        let timeout = Duration::from_secs(self.config.network.connect_timeout_secs);
        let conn = tokio::time::timeout(
            timeout,
            self.endpoint.connect(server_addr, "nidan-server")?,
        ).await
            .context("timeout connexion serveur")?
            .context("connexion QUIC serveur")?;

        info!(remote = %conn.remote_address(), "connexion QUIC serveur établie");

        // ── Handshake ──────────────────────────────────────────────────────
        let (ack, mut video_cipher, mut control_cipher) = Self::do_handshake(&conn, &self.config, &session_token).await
            .context("handshake serveur")?;

        // Priorité : config forcée > résolution annoncée par le serveur > défaut
        let width = self.config.display.force_width
            .unwrap_or(if ack.width > 0 { ack.width } else { 1280 });
        let height = self.config.display.force_height
            .unwrap_or(if ack.height > 0 { ack.height } else { 720 });

        info!(width, height, codec = ack.selected_codec, "handshake OK — démarrage session");

        // ── Pipeline ───────────────────────────────────────────────────────
        let (tx_video, rx_video) = mpsc::channel::<VideoFrame>(VIDEO_CHANNEL_SIZE);
        let (tx_decoded, rx_decoded_sync) = {
            let (s, r) = std::sync::mpsc::sync_channel::<DecodedFrame>(DECODED_CHANNEL_SIZE);
            // Adaptateur : tokio mpsc → std sync_channel pour SDL2
            let (ts, tr) = mpsc::channel::<DecodedFrame>(DECODED_CHANNEL_SIZE);
            (ts, r)
        };
        let (tx_input, rx_input)   = mpsc::channel::<InputEvent>(INPUT_CHANNEL_SIZE);
        let (tx_batch, mut rx_batch) = mpsc::channel::<InputBatch>(64);

        // Démarrage du décodeur
        let dec_pipeline = DecoderPipeline::new(
            self.config.video.hardware_decode,
            None, // clé E2E Phase 2.2
        );
        let dec_shutdown = shutdown.clone();
        let (tx_dec_in, rx_dec_in)     = mpsc::channel::<VideoFrame>(VIDEO_CHANNEL_SIZE);
        let (tx_dec_out, mut rx_dec_out) = mpsc::channel::<DecodedFrame>(DECODED_CHANNEL_SIZE);
        let _dec_handle = dec_pipeline.start(rx_dec_in, tx_dec_out, dec_shutdown);

        // Renderer SDL2 dans thread dédié (std::thread car SDL2)
        let (frame_tx_sdl, frame_rx_sdl) = std::sync::mpsc::sync_channel::<DecodedFrame>(4);
        let (input_tx_sdl, input_rx_sdl) = mpsc::channel::<InputEvent>(INPUT_CHANNEL_SIZE);
        let (metrics_tx, metrics_rx)     = tokio::sync::watch::channel(
            renderer::RenderMetrics::default()
        );
        let display_cfg = self.config.display.clone();
        let _renderer_thread = std::thread::spawn(move || {
            renderer::sdl::run_sdl2_loop(
                display_cfg, width, height,
                frame_rx_sdl, input_tx_sdl, metrics_tx,
            )
        });

        // InputSender : agrège les inputs et les envoie au serveur
        let input_sender = InputSender::new();
        let inp_shutdown  = shutdown.clone();
        let _inp_handle = input_sender.start(input_rx_sdl, tx_batch, inp_shutdown);

        // ── Boucle principale ──────────────────────────────────────────────
        // Stream vidéo entrant (unidirectionnel serveur → client)
        let mut video_rx = conn.accept_uni().await
            .context("acceptation stream vidéo QUIC")?;

        // Stream de contrôle bidirectionnel
        let (mut ctrl_tx, mut ctrl_rx) = conn.open_bi().await
            .context("ouverture stream contrôle QUIC")?;

        // Presse-papier local du poste : backend Wayland ou X11 selon la session.
        // Permet de coller (Ctrl+V) le presse-papier reçu du serveur. Best-effort.
        #[cfg(any(feature = "x11-clipboard", feature = "wayland-clipboard"))]
        let local_clipboard = crate::clipboard_local::LocalClipboard::detect_and_start();

        // QUIC n'expose le stream bi au pair qu'au premier octet écrit. On envoie
        // une trame d'ouverture (type CTRL_MSG_OPEN) pour que le serveur fasse
        // aboutir son accept_bi() immédiatement — sinon le sens serveur→client
        // (presse-papier) reste bloqué tant que le client n'a rien à envoyer.
        {
            let open_frame: [u8; 2] = [nidan_proto::CTRL_MSG_OPEN, 0u8];
            let _ = ctrl_tx.write_all(&(open_frame.len() as u32).to_be_bytes()).await;
            let _ = ctrl_tx.write_all(&open_frame).await;
        }

        // Hook de test : si NIDAN_TEST_CLIPBOARD est défini, envoyer son contenu
        // comme transfert de presse-papier (client → serveur) puis continuer.
        // Permet de valider le canal de bout en bout sans capture X réelle.
        if let Ok(test_clip) = std::env::var("NIDAN_TEST_CLIPBOARD") {
            // Ouvrir le stream de contrôle côté serveur en envoyant d'abord
            // une trame, puis le transfert clipboard.
            match Self::send_clipboard(
                &mut ctrl_tx,
                "test-session",
                nidan_proto::CLIP_MIME_TEXT_PLAIN,
                test_clip.as_bytes(),
                control_cipher.as_mut(),
            ).await {
                Ok(()) => info!(bytes = test_clip.len(), "presse-papier de test envoyé au serveur"),
                Err(e) => warn!(error = %e, "échec envoi presse-papier de test"),
            }
        }

        // ── Lecteurs de streams dans des tâches dédiées (CANCEL-SAFETY) ──────
        // read_exact n'est PAS cancel-safe : utilisé directement dans un
        // tokio::select!, une lecture interrompue entre le préfixe et le
        // payload perd les octets déjà lus et désynchronise le stream.
        // On déplace chaque lecture dans sa propre tâche qui lit en boucle et
        // pousse les trames via un channel ; le select! principal ne fait plus
        // que des recv() de channel, qui sont cancel-safe.
        let (tx_video_frame, mut rx_video_frame) = tokio::sync::mpsc::channel::<VideoFrame>(8);
        let video_reader = tokio::spawn(async move {
            loop {
                match Self::read_video_frame(&mut video_rx).await {
                    Ok(frame) => { if tx_video_frame.send(frame).await.is_err() { break; } }
                    Err(e) => { warn!(error = %e, "lecture frame vidéo arrêtée"); break; }
                }
            }
        });

        let (tx_ctrl_frame, mut rx_ctrl_frame) = tokio::sync::mpsc::channel::<(u8, u8, Vec<u8>)>(8);
        let ctrl_reader = tokio::spawn(async move {
            loop {
                match Self::read_control_frame(&mut ctrl_rx).await {
                    Ok(Some(frame)) => { if tx_ctrl_frame.send(frame).await.is_err() { break; } }
                    Ok(None) => continue,
                    Err(_) => break,
                }
            }
        });

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("arrêt demandé");
                    break;
                }

                _ = conn.closed() => {
                    warn!("connexion QUIC fermée par le serveur");
                    break;
                }

                // Réception d'une VideoFrame (depuis la tâche lectrice : cancel-safe)
                maybe_frame = rx_video_frame.recv() => {
                    match maybe_frame {
                        Some(mut frame) => {
                            // Déchiffrement E2E si la frame est chiffrée
                            if frame.encrypted {
                                if let Some(ref cipher) = video_cipher {
                                    match cipher.decrypt(&frame.encoded_data, &frame.nonce) {
                                        Ok(pt) => { frame.encoded_data = pt; frame.encrypted = false; }
                                        Err(e) => { warn!(error = %e, "déchiffrement frame échoué"); continue; }
                                    }
                                } else {
                                    warn!("frame chiffrée mais pas de clé E2E — ignorée");
                                    continue;
                                }
                            }
                            // Étape 6h bis : même mécanisme de protection que
                            // pour l'envoi des InputBatch (voir plus bas). Si
                            // le décodeur se bloque (frame corrompue, bug
                            // interne openh264/FFmpeg), ce canal borné (8
                            // emplacements) se remplit et ce .send().await
                            // bloquerait indéfiniment TOUTE la boucle select
                            // — vidéo ET inputs. Le décodage prend déjà
                            // 120-150ms/frame en usage normal (mesuré), donc
                            // un vrai blocage interne est plausible.
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                tx_dec_in.send(frame)
                            ).await {
                                Ok(Ok(())) => {}
                                Ok(Err(_)) => break,
                                Err(_) => {
                                    error!("timeout (5s) envoi frame au décodeur — \
                                            décodeur probablement bloqué, fin de session");
                                    break;
                                }
                            }
                        }
                        None => { warn!("flux vidéo terminé"); break; }
                    }
                }

                // Frame décodée → renderer SDL2
                Some(decoded) = rx_dec_out.recv() => {
                    let _ = frame_tx_sdl.try_send(decoded);
                }

                // InputBatch → envoi au serveur sur le stream de contrôle
                // Étape 6h : send_input_batch() fait des write_all() directs
                // sur le stream QUIC, SANS timeout. Si cette écriture se
                // bloque (contrôle de flux QUIC bloqué, stream qui ne se
                // vide plus), elle attend indéfiniment — et comme c'est
                // dans la MÊME boucle select! que la réception vidéo
                // (ci-dessus), toute la boucle se fige : vidéo ET inputs
                // en même temps. C'est le mécanisme exact du "freeze"
                // observé (confirmé par logs : proxy et agent arrêtent de
                // voir des InputBatch pile au moment où le client se fige,
                // alors que l'utilisateur continue de bouger la souris).
                Some(batch) = rx_batch.recv() => {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        Self::send_input_batch(&mut ctrl_tx, &batch, control_cipher.as_mut())
                    ).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            warn!(error = %e, "erreur envoi inputs");
                        }
                        Err(_) => {
                            error!("timeout (5s) envoi InputBatch sur stream de contrôle — \
                                    connexion probablement bloquée, fin de session");
                            break;
                        }
                    }
                }

                // Trame de contrôle (depuis la tâche lectrice : cancel-safe)
                maybe_ctrl = rx_ctrl_frame.recv() => {
                    if let Some((msg_type, flag, raw)) = maybe_ctrl {
                            // Déchiffrement si nécessaire (flag == 1)
                            let payload: Option<Vec<u8>> = if flag == 1 {
                                if raw.len() < 12 { None }
                                else {
                                    let nonce = &raw[..12];
                                    let ct = &raw[12..];
                                    match control_cipher.as_ref() {
                                        Some(c) => c.decrypt(ct, nonce).ok(),
                                        None => { warn!("trame contrôle chiffrée mais pas de clé"); None }
                                    }
                                }
                            } else {
                                Some(raw)
                            };
                            if let Some(payload) = payload {
                                if msg_type == nidan_proto::CTRL_MSG_CLIPBOARD {
                                    match serde_json::from_slice::<nidan_proto::ClipboardTransferRequest>(&payload) {
                                        Ok(req) => {
                                            info!(
                                                bytes = req.content.len(),
                                                "presse-papier reçu du serveur (serveur→client)"
                                            );
                                            // Écriture dans le presse-papier local → collable (Ctrl+V)
                                            #[cfg(any(feature = "x11-clipboard", feature = "wayland-clipboard"))]
                                            local_clipboard.set_content(&req.content);
                                        }
                                        Err(e) => warn!(error = %e, "décodage clipboard s2c"),
                                    }
                                }
                            }
                    }
                }
            }
        }

        // Arrêt des tâches lectrices
        video_reader.abort();
        ctrl_reader.abort();

        // Fermeture propre
        conn.close(0u32.into(), b"session ended");
        Ok(())
    }

    /// Connexion au broker pour obtenir session token + adresse serveur.
    ///
    /// Flux : QUIC vers le broker → ClientSessionRequest (auth) →
    /// BrokerSessionResponse (adresse VM + JWT). Le broker authentifie et
    /// attribue une VM, puis le client se connecte directement au serveur
    /// retourné (la session E2E reste opaque au broker).
    async fn connect_via_broker(&self) -> Result<(String, Vec<u8>)> {
        use nidan_proto::{ClientSessionRequest, BrokerSessionResponse, AuthResult};

        let broker_addr: SocketAddr = self.config.network.broker_addr.parse()
            .with_context(|| format!("adresse broker invalide: {}", self.config.network.broker_addr))?;

        info!(addr = %broker_addr, "connexion au broker");
        let timeout = Duration::from_secs(self.config.network.connect_timeout_secs);
        let conn = tokio::time::timeout(
            timeout,
            self.endpoint.connect(broker_addr, "nidan-broker")?,
        ).await
            .context("timeout connexion broker")?
            .context("connexion QUIC broker")?;

        // Ouvre un stream bi : envoie la requête, attend la réponse
        let (mut tx, mut rx) = conn.open_bi().await
            .context("ouverture stream broker")?;

        // auth_method : 1 = mTLS (le certificat client porte déjà l'identité)
        let request = ClientSessionRequest {
            client_version:   env!("CARGO_PKG_VERSION").to_string(),
            auth_method:      1, // mTLS
            auth_token:       vec![],
            preferred_vm_tag: self.config.network.preferred_vm_tag.clone(),
            session_label:    self.config.display.window_title.clone(),
            client_nonce:     vec![],
        };
        let req_bytes = serde_json::to_vec(&request)?;
        tx.write_all(&(req_bytes.len() as u32).to_be_bytes()).await?;
        tx.write_all(&req_bytes).await?;
        tx.finish().ok();

        // Réception de la réponse
        let mut len_buf = [0u8; 4];
        rx.read_exact(&mut len_buf).await.context("lecture longueur réponse broker")?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 8192 { anyhow::bail!("réponse broker trop grande: {len}"); }
        let mut buf = vec![0u8; len];
        rx.read_exact(&mut buf).await.context("lecture réponse broker")?;
        let resp: BrokerSessionResponse = nidan_proto::decode_message(&buf)
            .context("décodage réponse broker")?;

        // Fermeture propre de la connexion broker (la session passe en direct)
        conn.close(0u32.into(), b"broker handshake done");

        match resp.auth_result {
            x if x == AuthResult::Success as i32 => {
                if resp.server_address.is_empty() {
                    anyhow::bail!("broker : succès mais adresse serveur vide");
                }
                info!(
                    vm_id = %resp.vm_id,
                    server = %resp.server_address,
                    token_len = resp.session_token.len(),
                    "broker : VM attribuée, session autorisée"
                );
                // Le JWT sera présenté au serveur dans le handshake
                Ok((resp.server_address, resp.session_token))
            }
            x if x == AuthResult::MfaNeeded as i32 => {
                anyhow::bail!("broker : MFA requis ({})", resp.error_message);
            }
            x if x == AuthResult::Expired as i32 => {
                anyhow::bail!("broker : session expirée ({})", resp.error_message);
            }
            _ => {
                anyhow::bail!("broker : authentification refusée ({})", resp.error_message);
            }
        }
    }

    /// Handshake client → serveur
    async fn do_handshake(
        conn: &quinn::Connection,
        config: &ClientConfig,
        session_token: &[u8],
    ) -> Result<(ServerHandshakeAck, Option<nidan_common::crypto::StreamCipher>, Option<nidan_common::crypto::StreamCipher>)> {
        let (mut tx, mut rx) = conn.open_bi().await
            .context("ouverture stream handshake")?;

        // Génère la paire X25519 + nonce pour le chiffrement E2E
        use nidan_common::crypto::{KeyExchange, derive_session_keys, StreamCipher};
        let client_kx = KeyExchange::new();
        let client_nonce = nidan_common::crypto::random_bytes(32);

        let hs = ClientServerHandshake {
            session_id:          SessionId::new().0,
            preferred_codec:     1, // H264
            preferred_format:    1, // YUV420P
            target_fps:          config.video.max_fps,
            target_bitrate_kbps: config.video.target_bitrate_kbps,
            audio_enabled:       false,
            seamless_mode:       config.display.seamless,
            client_public_key:   if config.security.e2e_encryption { client_kx.public.to_vec() } else { vec![] },
            client_nonce:        if config.security.e2e_encryption { client_nonce.clone() } else { vec![] },
            session_token:       session_token.to_vec(),
            ..Default::default()
        };

        // Envoi length-prefixed : [len4][json] (pas de double préfixe)
        let data = serde_json::to_vec(&hs).context("sérialisation handshake")?;
        tx.write_all(&(data.len() as u32).to_be_bytes()).await
            .context("écriture len handshake")?;
        tx.write_all(&data).await.context("écriture handshake")?;
        tx.finish().context("flush handshake")?;

        // Réception ACK
        let mut len_buf = [0u8; 4];
        rx.read_exact(&mut len_buf).await.context("lecture len ACK")?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len > 4096 { bail!("ACK trop grand: {} bytes", len); }

        let mut buf = vec![0u8; len];
        rx.read_exact(&mut buf).await.context("lecture payload ACK")?;

        let ack = nidan_proto::decode_message::<ServerHandshakeAck>(&buf).context("décodage ACK")?;

        // Si le serveur a activé le E2E, dériver le cipher vidéo
        let (video_cipher, control_cipher) = if ack.e2e_enabled && !ack.server_public_key.is_empty() {
            let secret = client_kx.shared_secret(&ack.server_public_key)
                .context("ECDH côté client")?;
            let keys = derive_session_keys(&secret, &client_nonce, &ack.server_nonce)
                .context("dérivation clés client")?;
            info!("chiffrement E2E actif (X25519 + ChaCha20-Poly1305) — vidéo + inputs");
            (Some(StreamCipher::new(&keys.video)), Some(StreamCipher::new(&keys.control)))
        } else {
            (None, None)
        };

        Ok((ack, video_cipher, control_cipher))
    }

    /// Lit une VideoFrame length-prefixed depuis un stream QUIC
    /// Lit une trame du canal de contrôle : `[len][msg_type][flag][reste]`.
    /// Retourne (msg_type, flag, reste-brut) ; le déchiffrement est fait par
    /// l'appelant (qui détient la clé). Retourne Ok(None) si trame invalide.
    async fn read_control_frame(
        rx: &mut quinn::RecvStream,
    ) -> Result<Option<(u8, u8, Vec<u8>)>> {
        let mut len_buf = [0u8; 4];
        rx.read_exact(&mut len_buf).await.context("lecture len trame contrôle")?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len < 2 || len > 1_100_000 { return Ok(None); }
        let mut buf = vec![0u8; len];
        rx.read_exact(&mut buf).await.context("lecture trame contrôle")?;
        let msg_type = buf[0];
        let flag = buf[1];
        Ok(Some((msg_type, flag, buf[2..].to_vec())))
    }

    async fn read_video_frame(rx: &mut quinn::RecvStream) -> Result<VideoFrame> {
        let mut len_buf = [0u8; 4];
        rx.read_exact(&mut len_buf).await
            .context("lecture longueur frame")?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len > nidan_proto::MAX_VIDEO_FRAME_BYTES {
            bail!("frame trop grande: {} bytes", len);
        }

        let mut buf = vec![0u8; len];
        rx.read_exact(&mut buf).await.context("lecture payload frame")?;

        nidan_proto::decode_message::<VideoFrame>(&buf).context("décodage VideoFrame")
    }

    /// Envoie un InputBatch sur le stream de contrôle
    async fn send_input_batch(
        tx: &mut quinn::SendStream,
        batch: &InputBatch,
        cipher: Option<&mut nidan_common::crypto::StreamCipher>,
    ) -> Result<()> {
        let json = serde_json::to_vec(batch)?;
        // Format fil : [msg_type 1o][flag 1o][si chiffré: nonce 12o][payload]
        // msg_type = CTRL_MSG_INPUT, flag = 0 → JSON clair, flag = 1 → ChaCha20-Poly1305
        let framed = Self::frame_control_message(
            nidan_proto::CTRL_MSG_INPUT, &json, cipher)?;
        tx.write_all(&(framed.len() as u32).to_be_bytes()).await?;
        tx.write_all(&framed).await?;
        Ok(())
    }

    /// Construit une trame de canal de contrôle : `[msg_type][flag][...]`.
    /// Réutilisé pour les inputs et le presse-papier (même chiffrement E2E).
    fn frame_control_message(
        msg_type: u8,
        payload: &[u8],
        cipher: Option<&mut nidan_common::crypto::StreamCipher>,
    ) -> Result<Vec<u8>> {
        let body = match cipher {
            Some(c) => {
                let (ct, nonce) = c.encrypt(payload)
                    .map_err(|e| anyhow::anyhow!("chiffrement control: {e}"))?;
                let mut out = Vec::with_capacity(2 + nonce.len() + ct.len());
                out.push(msg_type);
                out.push(1u8); // chiffré
                out.extend_from_slice(&nonce);
                out.extend_from_slice(&ct);
                out
            }
            None => {
                let mut out = Vec::with_capacity(2 + payload.len());
                out.push(msg_type);
                out.push(0u8); // clair
                out.extend_from_slice(payload);
                out
            }
        };
        Ok(body)
    }

    /// Envoie un transfert de presse-papier au serveur sur le canal de contrôle.
    /// Le filtrage de politique est appliqué côté serveur (et idéalement ici aussi).
    async fn send_clipboard(
        tx: &mut quinn::SendStream,
        session_id: &str,
        mime_code: i32,
        content: &[u8],
        cipher: Option<&mut nidan_common::crypto::StreamCipher>,
    ) -> Result<()> {
        use nidan_proto::ClipboardTransferRequest;
        let req = ClipboardTransferRequest {
            session_id:   session_id.to_string(),
            direction:    nidan_proto::CLIP_DIR_CLIENT_TO_SERVER,
            mime_type:    mime_code,
            content:      content.to_vec(),
            content_hash: 0,
            size_bytes:   content.len() as u32,
        };
        let json = serde_json::to_vec(&req)?;
        let framed = Self::frame_control_message(
            nidan_proto::CTRL_MSG_CLIPBOARD, &json, cipher)?;
        tx.write_all(&(framed.len() as u32).to_be_bytes()).await?;
        tx.write_all(&framed).await?;
        Ok(())
    }
}
