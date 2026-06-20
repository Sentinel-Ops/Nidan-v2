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

        let client_config = quinn::ClientConfig::new(Arc::new(tls_config));

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
        let server_addr = if let Some(ref direct) = self.config.network.direct_server {
            // Mode direct (dev) : bypass broker
            info!(addr = %direct, "connexion directe au serveur (mode dev)");
            direct.clone()
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
        let ack = Self::do_handshake(&conn, &self.config).await
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

                // Réception d'une VideoFrame depuis le serveur
                result = Self::read_video_frame(&mut video_rx) => {
                    match result {
                        Ok(frame) => {
                            if tx_dec_in.send(frame).await.is_err() { break; }
                        }
                        Err(e) => {
                            warn!(error = %e, "erreur lecture frame vidéo");
                            break;
                        }
                    }
                }

                // Frame décodée → renderer SDL2
                Some(decoded) = rx_dec_out.recv() => {
                    let _ = frame_tx_sdl.try_send(decoded);
                }

                // InputBatch → envoi au serveur sur le stream de contrôle
                Some(batch) = rx_batch.recv() => {
                    if let Err(e) = Self::send_input_batch(&mut ctrl_tx, &batch).await {
                        warn!(error = %e, "erreur envoi inputs");
                    }
                }
            }
        }

        // Fermeture propre
        conn.close(0u32.into(), b"session ended");
        Ok(())
    }

    /// Connexion au broker pour obtenir session token + adresse serveur
    async fn connect_via_broker(&self) -> Result<String> {
        // TODO Phase 2.1 : implémentation complète
        // Pour l'instant : utilise l'adresse broker directement comme serveur
        warn!("connexion broker non implémentée — connexion directe via adresse broker");
        Ok(self.config.network.broker_addr.clone())
    }

    /// Handshake client → serveur
    async fn do_handshake(
        conn: &quinn::Connection,
        config: &ClientConfig,
    ) -> Result<ServerHandshakeAck> {
        let (mut tx, mut rx) = conn.open_bi().await
            .context("ouverture stream handshake")?;

        let hs = ClientServerHandshake {
            session_id:          SessionId::new().0,
            preferred_codec:     1, // H264
            preferred_format:    1, // YUV420P
            target_fps:          config.video.max_fps,
            target_bitrate_kbps: config.video.target_bitrate_kbps,
            audio_enabled:       false,
            seamless_mode:       config.display.seamless,
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

        nidan_proto::decode_message::<ServerHandshakeAck>(&buf).context("décodage ACK")
    }

    /// Lit une VideoFrame length-prefixed depuis un stream QUIC
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
    ) -> Result<()> {
        let data = serde_json::to_vec(batch)?;
        tx.write_all(&(data.len() as u32).to_be_bytes()).await?;
        tx.write_all(&data).await?;
        Ok(())
    }
}
