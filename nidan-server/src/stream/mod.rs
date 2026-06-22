//! Serveur QUIC NIDAN — orchestration du pipeline complet.
//!
//! ## Flux d'une connexion
//! ```text
//! Client TCP/QUIC connect
//!        ↓
//! [TLS handshake mTLS]
//!        ↓
//! [Réception ClientServerHandshake proto]
//!        ↓
//! [Démarrage pipeline capture + encodage]
//!        ↓
//! [Stream QUIC unidirectionnel : VideoFrame proto]
//!        ↓
//! [Stream QUIC bidirectionnel : ControlMessage]
//!        ↓
//! [Stream QUIC unidirectionnel (optionnel) : AudioFrame]
//! ```

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use nidan_common::session::SessionId;
use nidan_proto::{
    ClientServerHandshake, ServerHandshakeAck, SessionState, VideoFrame,
};

use crate::capture::{create_capturer, RawFrame};
use crate::config::ServerConfig;
use crate::encoder::{CodecChoice, EncoderParams, EncoderPipeline};
use crate::session::ServerSession;

/// Taille des channels internes (frames en buffer)
const CAPTURE_CHANNEL_SIZE: usize = 8;
const ENCODE_CHANNEL_SIZE:  usize = 4;

/// Serveur QUIC NIDAN
pub struct QuicServer {
    config: ServerConfig,
    display: u32,
    endpoint: quinn::Endpoint,
}

impl QuicServer {
    /// Crée et initialise le serveur QUIC
    pub async fn new(config: ServerConfig, display: u32) -> Result<Self> {
        let bind_addr: SocketAddr = config.network.bind_addr
            .parse()
            .with_context(|| format!("adresse de bind invalide: {}", config.network.bind_addr))?;

        // Configuration TLS serveur
        let tls_config = Self::build_tls_config(&config)
            .context("configuration TLS")?;

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
        let endpoint = quinn::Endpoint::server(server_config, bind_addr)
            .with_context(|| format!("bind QUIC sur {}", bind_addr))?;

        info!(addr = %bind_addr, "serveur QUIC en écoute");

        Ok(Self { config, display, endpoint })
    }

    /// Construit la configuration TLS/QUIC depuis les certificats
    fn build_tls_config(config: &ServerConfig) -> Result<quinn::crypto::rustls::QuicServerConfig> {
        use std::fs;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use rustls_pemfile::{certs as parse_certs, pkcs8_private_keys};

        let cert_pem = fs::read(&config.tls.cert)
            .with_context(|| format!("lecture cert: {}", config.tls.cert))?;
        let server_certs: Vec<CertificateDer> = parse_certs(&mut cert_pem.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .context("parsing cert PEM")?;

        let key_pem = fs::read(&config.tls.key)
            .with_context(|| format!("lecture clé: {}", config.tls.key))?;
        let mut keys = pkcs8_private_keys(&mut key_pem.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .context("parsing clé PEM")?;
        if keys.is_empty() {
            anyhow::bail!("aucune clé PKCS8 dans {}", config.tls.key);
        }

        let ca_pem = fs::read(&config.tls.ca_cert)
            .with_context(|| format!("lecture CA: {}", config.tls.ca_cert))?;
        let ca_certs: Vec<CertificateDer> = parse_certs(&mut ca_pem.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .context("parsing CA PEM")?;

        let mut root_store = rustls::RootCertStore::empty();
        for ca in ca_certs {
            root_store.add(ca).context("ajout CA")?;
        }

        let client_verifier = rustls::server::WebPkiClientVerifier::builder(
            Arc::new(root_store)
        ).build().context("client verifier mTLS")?;

        let tls = rustls::ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(server_certs, PrivateKeyDer::Pkcs8(keys.remove(0)))
            .context("configuration TLS serveur")?;

        quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .context("QuicServerConfig")
    }

    /// Boucle principale : accepte les connexions QUIC
    pub async fn run(self) -> Result<()> {
        let shutdown = tokio_util::sync::CancellationToken::new();

        // Gestionnaire de signal SIGTERM/SIGINT
        let shutdown_clone = shutdown.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("signal d'arrêt reçu");
            shutdown_clone.cancel();
        });

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("arrêt du serveur QUIC");
                    self.endpoint.close(0u32.into(), b"server shutdown");
                    break;
                }
                incoming = self.endpoint.accept() => {
                    match incoming {
                        Some(conn) => {
                            let config = self.config.clone();
                            let display = self.display;
                            let shutdown = shutdown.clone();

                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_connection(conn, config, display, shutdown).await {
                                    error!(error = %e, "erreur connexion");
                                }
                            });
                        }
                        None => {
                            info!("endpoint QUIC fermé");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Gère une connexion QUIC entrante
    async fn handle_connection(
        incoming: quinn::Incoming,
        config: ServerConfig,
        display: u32,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let conn = incoming.await.context("acceptation connexion QUIC")?;
        let remote = conn.remote_address();
        info!(remote = %remote, "nouvelle connexion QUIC");

        // 1. Handshake : réception du ClientServerHandshake (garde le stream pour l'ACK)
        let (handshake, mut hs_tx) = Self::receive_handshake(&conn).await
            .context("réception handshake")?;

        // Vérification du jeton de session délivré par le broker (si exigé).
        // Empêche un client de contourner le broker en se connectant directement.
        if config.security.require_session_token {
            let token = String::from_utf8_lossy(&handshake.session_token);
            match crate::session_token::verify_session_token(&token, &config.security.jwt_secret) {
                Ok(claims) => {
                    info!(
                        user = %claims.sub, vm = %claims.vm_id,
                        "jeton de session broker validé"
                    );
                }
                Err(e) => {
                    warn!(error = %e, client = %remote, "jeton de session refusé — session rejetée");
                    let nack = ServerHandshakeAck {
                        accepted: false,
                        error_message: "jeton de session invalide ou absent".to_string(),
                        ..Default::default()
                    };
                    let _ = Self::send_ack(&mut hs_tx, &nack).await;
                    anyhow::bail!("jeton de session refusé");
                }
            }
        }

        let session_id = SessionId::new();
        info!(session_id = %session_id, client = %remote, "session démarrée");

        // 2. Envoi de l'ACK
        let caps = crate::capture::create_capturer(&config.capture.backend, display, config.capture.use_xshm, config.capture.use_xdamage, config.capture.portal_restore_token.clone())
            .map(|c| c.capabilities().clone())
            .ok();

        // Résolution réelle capturée (défaut 1280x720 si caps indisponibles)
        let (cap_w, cap_h) = caps.as_ref()
            .map(|c| (c.width, c.height))
            .unwrap_or((1280, 720));

        // Échange de clés E2E (X25519) si le client a fourni sa clé publique
        let e2e_enabled = config.security.e2e_encryption
            && !handshake.client_public_key.is_empty();

        let (server_kx, server_nonce, video_cipher, control_cipher) = if e2e_enabled {
            use nidan_common::crypto::{KeyExchange, derive_session_keys, StreamCipher};
            let kx = KeyExchange::new();
            let server_nonce = nidan_common::crypto::random_bytes(32);
            match kx.shared_secret(&handshake.client_public_key) {
                Ok(secret) => {
                    let keys = derive_session_keys(&secret, &handshake.client_nonce, &server_nonce)
                        .context("dérivation clés de session")?;
                    info!(session_id = %session_id, "chiffrement E2E activé (X25519 + ChaCha20-Poly1305) — vidéo + inputs");
                    (Some(kx), server_nonce,
                     Some(StreamCipher::new(&keys.video)),
                     Some(StreamCipher::new(&keys.control)))
                }
                Err(e) => {
                    warn!(error = %e, "échec ECDH — session en clair");
                    (None, vec![], None, None)
                }
            }
        } else {
            (None, vec![], None, None)
        };

        let ack = ServerHandshakeAck {
            accepted: true,
            selected_codec: handshake.preferred_codec,
            selected_format: 1, // YUV420P
            state: SessionState::Active as i32,
            stream_id: 1,
            width: cap_w,
            height: cap_h,
            server_public_key: server_kx.as_ref().map(|k| k.public.to_vec()).unwrap_or_default(),
            server_nonce,
            e2e_enabled: video_cipher.is_some(),
            ..Default::default()
        };

        Self::send_ack(&mut hs_tx, &ack).await
            .context("envoi handshake ACK")?;

        // 3. Démarrage du pipeline capture → encodage → stream
        Self::run_session(conn, config, display, session_id, handshake, video_cipher, control_cipher, shutdown).await
    }

    /// Réceptionne le handshake initial du client
    async fn receive_handshake(
        conn: &quinn::Connection,
    ) -> Result<(ClientServerHandshake, quinn::SendStream)> {
        // Le client ouvre UN stream bi : il envoie le handshake puis attend l'ACK
        // sur le même stream. On garde donc la moitié SEND pour répondre.
        let (tx, mut rx) = conn.accept_bi().await
            .context("ouverture stream bi pour handshake")?;

        let mut len_buf = [0u8; 4];
        rx.read_exact(&mut len_buf).await.context("lecture longueur handshake")?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 4096 {
            anyhow::bail!("handshake trop grand: {} bytes", len);
        }
        let mut buf = vec![0u8; len];
        rx.read_exact(&mut buf).await.context("lecture payload handshake")?;

        let hs = nidan_proto::decode_message::<ClientServerHandshake>(&buf)
            .context("décodage proto ClientServerHandshake")?;
        Ok((hs, tx))
    }

    /// Envoie l'ACK sur le MÊME stream que le handshake
    async fn send_ack(tx: &mut quinn::SendStream, ack: &ServerHandshakeAck) -> Result<()> {
        let data = serde_json::to_vec(ack).context("sérialisation ACK")?;
        let len = (data.len() as u32).to_be_bytes();
        tx.write_all(&len).await.context("écriture longueur ACK")?;
        tx.write_all(&data).await.context("écriture payload ACK")?;
        Ok(())
    }

    /// Envoie le presse-papier de la VM vers le client (sens serveur → client),
    /// après filtrage par la politique. Trame : [CTRL_MSG_CLIPBOARD][flag][...].
    /// Le chiffrement utilise `encrypt_dir(.., 1)` pour isoler le nonce du sens
    /// retour de celui du sens aller.
    async fn send_clipboard_to_client(
        tx: &mut quinn::SendStream,
        filter: &nidan_common::clipboard::ClipboardFilter,
        session_id: &str,
        content: &[u8],
        cipher: Option<&mut nidan_common::crypto::StreamCipher>,
    ) {
        use nidan_proto::ClipboardTransferRequest;
        // Filtrage côté serveur AVANT émission (sens serveur → client)
        let mime = nidan_proto::clip_mime_to_str(nidan_proto::CLIP_MIME_TEXT_PLAIN);
        let decision = filter.evaluate_proto(
            nidan_proto::CLIP_DIR_SERVER_TO_CLIENT, mime, content);
        match decision {
            nidan_common::clipboard::ClipboardDecision::Block(reason) => {
                tracing::warn!(
                    session = %session_id, size = content.len(), reason = %reason,
                    "presse-papier serveur→client REFUSÉ par le filtre (non émis)"
                );
                return;
            }
            nidan_common::clipboard::ClipboardDecision::Allow => {
                if filter.audit_enabled() {
                    tracing::info!(
                        session = %session_id, size = content.len(),
                        "presse-papier serveur→client accepté (filtre OK)"
                    );
                }
            }
        }

        let req = ClipboardTransferRequest {
            session_id:   session_id.to_string(),
            direction:    nidan_proto::CLIP_DIR_SERVER_TO_CLIENT,
            mime_type:    nidan_proto::CLIP_MIME_TEXT_PLAIN,
            content:      content.to_vec(),
            content_hash: 0,
            size_bytes:   content.len() as u32,
        };
        let json = match serde_json::to_vec(&req) {
            Ok(j) => j,
            Err(e) => { tracing::warn!(error = %e, "sérialisation clipboard s2c"); return; }
        };

        // Framing : [msg_type][flag][si chiffré: nonce 12o][payload]
        let framed: Vec<u8> = match cipher {
            Some(c) => match c.encrypt_dir(&json, 1) {
                Ok((ct, nonce)) => {
                    let mut out = Vec::with_capacity(2 + nonce.len() + ct.len());
                    out.push(nidan_proto::CTRL_MSG_CLIPBOARD);
                    out.push(1u8);
                    out.extend_from_slice(&nonce);
                    out.extend_from_slice(&ct);
                    out
                }
                Err(e) => { tracing::warn!(error = %e, "chiffrement clipboard s2c"); return; }
            },
            None => {
                let mut out = Vec::with_capacity(2 + json.len());
                out.push(nidan_proto::CTRL_MSG_CLIPBOARD);
                out.push(0u8);
                out.extend_from_slice(&json);
                out
            }
        };

        let len = (framed.len() as u32).to_be_bytes();
        if tx.write_all(&len).await.is_err() || tx.write_all(&framed).await.is_err() {
            tracing::warn!("échec envoi presse-papier serveur→client");
        } else {
            tracing::info!(session = %session_id, bytes = content.len(),
                "presse-papier serveur→client émis");
        }
    }

    /// Boucle principale de session
    async fn run_session(
        conn: quinn::Connection,
        config: ServerConfig,
        display: u32,
        session_id: SessionId,
        handshake: ClientServerHandshake,
        mut video_cipher: Option<nidan_common::crypto::StreamCipher>,
        control_cipher: Option<nidan_common::crypto::StreamCipher>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let session_shutdown = tokio_util::sync::CancellationToken::new();

        // Channels entre les composants du pipeline
        let (tx_raw, rx_raw) = mpsc::channel::<RawFrame>(CAPTURE_CHANNEL_SIZE);
        let (tx_enc, mut rx_enc) = mpsc::channel::<crate::encoder::EncodedFrame>(ENCODE_CHANNEL_SIZE);

        // Codec sélectionné
        let codec = CodecChoice::from_str(&config.video.codec)
            .unwrap_or(CodecChoice::H264);

        // Capacités du capturer
        let capturer = create_capturer(&config.capture.backend, display, config.capture.use_xshm, config.capture.use_xdamage, config.capture.portal_restore_token.clone())
            .context("création capturer")?;

        let caps = capturer.capabilities().clone();

        let enc_params = EncoderParams::for_remote_desktop(
            codec,
            caps.width,
            caps.height,
            config.video.max_fps,
        );

        // Démarrage du capturer
        let cap_shutdown = session_shutdown.clone();
        let cap_handle = capturer.start(tx_raw, config.video.max_fps, cap_shutdown.clone());

        // Démarrage de l'encodeur
        let enc_pipeline = EncoderPipeline::new(enc_params, None /* clé E2E Phase 2 */);
        let enc_shutdown = session_shutdown.clone();
        let enc_handle = enc_pipeline.start(rx_raw, tx_enc, enc_shutdown);

        // Stream QUIC unidirectionnel pour le flux vidéo
        let mut video_tx = conn.open_uni().await
            .context("ouverture stream vidéo QUIC")?;

        // Tâche de réception des inputs client → injection XTEST
        let input_conn = conn.clone();
        let input_shutdown = session_shutdown.clone();
        let inj_display = display;
        let inj_w = caps.width;
        let inj_h = caps.height;
        let input_cipher = control_cipher;
        // Chiffreur dédié au sens retour (serveur → client) : même clé de
        // contrôle, compteur isolé, séparation des nonces via le marqueur de
        // direction (encrypt_dir). Évite toute réutilisation de nonce.
        let input_cipher_s2c = input_cipher.as_ref().map(|c| c.duplicate_fresh());
        // Filtre de presse-papier construit depuis la politique de session.
        let clip_filter = nidan_common::clipboard::ClipboardFilter::new(config.clipboard.clone());
        tracing::info!(
            c2s = config.clipboard.allow_client_to_server,
            s2c = config.clipboard.allow_server_to_client,
            patterns = config.clipboard.blocked_patterns.len(),
            "filtre presse-papier actif (canal de contrôle)"
        );
        let clip_session_id = session_id.to_string();
        tokio::spawn(async move {
            // Accepter le stream de contrôle bi-directionnel ouvert par le client.
            // On conserve AUSSI la moitié émission (tx) pour le presse-papier
            // serveur → client (sens retour).
            let (mut ctrl_tx, mut ctrl_rx) = match input_conn.accept_bi().await {
                Ok(pair) => pair,
                Err(e) => { tracing::warn!(error = %e, "pas de stream de contrôle inputs"); return; }
            };

            // Hook de test : si NIDAN_TEST_CLIPBOARD_S2C est défini, envoyer son
            // contenu vers le client après filtrage (sens serveur → client).
            if let Ok(test_clip) = std::env::var("NIDAN_TEST_CLIPBOARD_S2C") {
                let mut clip_cipher = input_cipher_s2c;
                Self::send_clipboard_to_client(
                    &mut ctrl_tx, &clip_filter, &clip_session_id,
                    test_clip.as_bytes(), clip_cipher.as_mut(),
                ).await;
            }
            let mut injector = match crate::input::InputInjector::new(inj_display, inj_w, inj_h) {
                Ok(i) => i,
                Err(e) => { tracing::warn!(error = %e, "injecteur indisponible"); return; }
            };
            tracing::info!("réception d'inputs démarrée");
            loop {
                if input_shutdown.is_cancelled() { break; }
                // Lire un InputBatch length-prefixed
                let mut len_buf = [0u8; 4];
                if ctrl_rx.read_exact(&mut len_buf).await.is_err() { break; }
                let len = u32::from_be_bytes(len_buf) as usize;
                if len == 0 || len > 65536 { continue; }
                let mut buf = vec![0u8; len];
                if ctrl_rx.read_exact(&mut buf).await.is_err() { break; }
                if buf.is_empty() { continue; }

                // Format : [msg_type 1o][flag 1o][si chiffré: nonce 12o][payload]
                if buf.len() < 2 { tracing::debug!("trame de contrôle trop courte"); continue; }
                let msg_type = buf[0];
                let flag = buf[1];
                let payload: Vec<u8> = if flag == 1 {
                    // Chiffré : nonce (12o) + ciphertext, après les 2 octets d'en-tête
                    if buf.len() < 2 + 12 { tracing::debug!("trame chiffrée trop courte"); continue; }
                    let nonce = &buf[2..14];
                    let ct = &buf[14..];
                    match input_cipher.as_ref() {
                        Some(cipher) => match cipher.decrypt(ct, nonce) {
                            Ok(pt) => pt,
                            Err(e) => { tracing::warn!(error = %e, "déchiffrement contrôle échoué"); continue; }
                        },
                        None => { tracing::warn!("trame chiffrée mais pas de clé contrôle"); continue; }
                    }
                } else {
                    // En clair : payload = tout après les 2 octets d'en-tête
                    buf[2..].to_vec()
                };

                match msg_type {
                    nidan_proto::CTRL_MSG_INPUT => {
                        match serde_json::from_slice::<nidan_proto::InputBatch>(&payload) {
                            Ok(batch) => { let _ = injector.inject_batch(&batch); }
                            Err(e) => tracing::debug!(error = %e, "décodage InputBatch"),
                        }
                    }
                    nidan_proto::CTRL_MSG_CLIPBOARD => {
                        match serde_json::from_slice::<nidan_proto::ClipboardTransferRequest>(&payload) {
                            Ok(req) => {
                                let mime = nidan_proto::clip_mime_to_str(req.mime_type);
                                let decision = clip_filter.evaluate_proto(
                                    req.direction, mime, &req.content);
                                match decision {
                                    nidan_common::clipboard::ClipboardDecision::Allow => {
                                        if clip_filter.audit_enabled() {
                                            tracing::info!(
                                                session = %clip_session_id,
                                                mime = %mime,
                                                size = req.content.len(),
                                                "presse-papier accepté (filtre OK)"
                                            );
                                        }
                                        // Injection effective dans la VM (sélection X)
                                        let _ = injector.set_clipboard(&req.content);
                                    }
                                    nidan_common::clipboard::ClipboardDecision::Block(reason) => {
                                        tracing::warn!(
                                            session = %clip_session_id,
                                            mime = %mime,
                                            size = req.content.len(),
                                            reason = %reason,
                                            "presse-papier REFUSÉ par le filtre"
                                        );
                                    }
                                }
                            }
                            Err(e) => tracing::debug!(error = %e, "décodage ClipboardTransfer"),
                        }
                    }
                    other => tracing::debug!(msg_type = other, "type de message de contrôle inconnu"),
                }
            }
            tracing::info!(injected = injector.injected_count(), "réception d'inputs terminée");
        });

        info!(session_id = %session_id, "pipeline démarré — streaming vidéo");
        let mut frames_encrypted: u64 = 0;
        let mut frames_total: u64 = 0;

        // Boucle de streaming : enc → proto → QUIC
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!(session_id = %session_id, "arrêt global");
                    session_shutdown.cancel();
                    break;
                }
                _ = conn.closed() => {
                    info!(session_id = %session_id, "client déconnecté");
                    session_shutdown.cancel();
                    break;
                }
                frame = rx_enc.recv() => {
                    match frame {
                        None => {
                            warn!(session_id = %session_id, "pipeline encodage terminé");
                            break;
                        }
                        Some(f) => {
                            let mut proto_frame = f.into_proto(0);
                            // Chiffrement E2E de la charge vidéo si activé
                            if let Some(ref mut cipher) = video_cipher {
                                match cipher.encrypt(&proto_frame.encoded_data) {
                                    Ok((ct, nonce)) => {
                                        proto_frame.encoded_data = ct;
                                        proto_frame.nonce = nonce.to_vec();
                                        proto_frame.encrypted = true;
                                    }
                                    Err(e) => { tracing::error!(error = %e, "chiffrement frame"); continue; }
                                }
                            }
                            let data = serde_json::to_vec(&proto_frame)?;
                            let len = (data.len() as u32).to_be_bytes();

                            // Length-prefixed protobuf sur stream QUIC
                            if let Err(e) = video_tx.write_all(&len).await {
                                warn!(error = %e, "erreur écriture stream vidéo");
                                break;
                            }
                            if let Err(e) = video_tx.write_all(&data).await {
                                warn!(error = %e, "erreur écriture payload vidéo");
                                break;
                            }
                            frames_total += 1;
                            if proto_frame.encrypted { frames_encrypted += 1; }
                            if frames_total % 60 == 0 {
                                info!(session_id = %session_id, total = frames_total,
                                      chiffrees = frames_encrypted,
                                      "frames envoyées");
                            }
                        }
                    }
                }
            }
        }

        session_shutdown.cancel();
        let _ = cap_handle.await;
        let _ = enc_handle.await;

        info!(session_id = %session_id, "session terminée");
        Ok(())
    }
}
