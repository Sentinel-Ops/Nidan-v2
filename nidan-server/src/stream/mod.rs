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

        let session_id = SessionId::new();
        info!(session_id = %session_id, client = %remote, "session démarrée");

        // 2. Envoi de l'ACK
        let caps = crate::capture::create_capturer(display, config.capture.use_xshm, config.capture.use_xdamage)
            .map(|c| c.capabilities().clone())
            .ok();

        let ack = ServerHandshakeAck {
            accepted: true,
            selected_codec: handshake.preferred_codec,
            selected_format: 1, // YUV420P
            state: SessionState::Active as i32,
            stream_id: 1,
            ..Default::default()
        };

        Self::send_ack(&mut hs_tx, &ack).await
            .context("envoi handshake ACK")?;

        // 3. Démarrage du pipeline capture → encodage → stream
        Self::run_session(conn, config, display, session_id, handshake, shutdown).await
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

    /// Boucle principale de session
    async fn run_session(
        conn: quinn::Connection,
        config: ServerConfig,
        display: u32,
        session_id: SessionId,
        handshake: ClientServerHandshake,
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
        let capturer = create_capturer(display, config.capture.use_xshm, config.capture.use_xdamage)
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
        tokio::spawn(async move {
            // Accepter le stream de contrôle bi-directionnel ouvert par le client
            let mut ctrl_rx = match input_conn.accept_bi().await {
                Ok((_tx, rx)) => rx,
                Err(e) => { tracing::warn!(error = %e, "pas de stream de contrôle inputs"); return; }
            };
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
                match serde_json::from_slice::<nidan_proto::InputBatch>(&buf) {
                    Ok(batch) => { let _ = injector.inject_batch(&batch); }
                    Err(e) => tracing::debug!(error = %e, "décodage InputBatch"),
                }
            }
            tracing::info!(injected = injector.injected_count(), "réception d'inputs terminée");
        });

        info!(session_id = %session_id, "pipeline démarré — streaming vidéo");

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
                            let proto_frame = f.into_proto(0);
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
