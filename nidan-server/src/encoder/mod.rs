//! Pipeline d'encodage vidéo via FFmpeg.
//!
//! Consomme des `RawFrame` depuis un channel, encode en H.264/H.265/AV1,
//! chiffre chaque frame avec ChaCha20-Poly1305, et envoie les `VideoFrame`
//! proto sur le channel de sortie.
//!
//! ## Architecture du pipeline
//! ```text
//! [RawFrame channel]
//!        ↓
//! [Conversion pixel format → YUV420P]
//!        ↓
//! [Encodeur FFmpeg (x264 / x265 / libaom-av1)]
//!     ↓ hardware path         ↓ software path
//! [NVENC/VAAPI/DXGI]     [libx264/libx265/libaom]
//!        ↓
//! [Paquet encodé (NAL units)]
//!        ↓
//! [Chiffrement ChaCha20-Poly1305]
//!        ↓
//! [VideoFrame proto → channel de sortie]
//! ```

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use nidan_common::crypto::{SessionKey, StreamCipher};
use nidan_proto::{VideoCodec, VideoFrame};

use crate::capture::RawFrame;

pub mod ffmpeg;
pub mod openh264_enc;
pub mod params;

pub use params::EncoderParams;

/// Codec d'encodage sélectionné
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CodecChoice {
    H264,
    H265,
    Av1,
}

impl CodecChoice {
    pub fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "h264" | "avc"  => Ok(Self::H264),
            "h265" | "hevc" => Ok(Self::H265),
            "av1"           => Ok(Self::Av1),
            other => bail!("codec inconnu: {other}"),
        }
    }

    pub fn to_proto_i32(self) -> i32 {
        match self {
            Self::H264 => nidan_proto::VideoCodec::H264 as i32,
            Self::H265 => nidan_proto::VideoCodec::H265 as i32,
            Self::Av1  => nidan_proto::VideoCodec::Av1  as i32,
        }
    }
}

/// Une frame encodée et prête à être transmise
#[derive(Debug)]
pub struct EncodedFrame {
    /// Payload NAL chiffré
    pub data: Vec<u8>,
    /// Nonce ChaCha20-Poly1305 (12 bytes)
    pub nonce: [u8; 12],
    /// Numéro de séquence
    pub seq: u64,
    /// Timestamp présentation (ms depuis début session)
    pub pts_ms: u32,
    /// Codec utilisé
    pub codec: CodecChoice,
    /// Keyframe / IDR
    pub is_keyframe: bool,
    /// Résolution
    pub width: u32,
    pub height: u32,
    /// Durée d'encodage (µs) pour métriques
    pub encode_duration_us: u32,
}

impl EncodedFrame {
    /// Convertit en message proto `VideoFrame`
    pub fn into_proto(self, monitor_index: u32) -> VideoFrame {
        VideoFrame {
            frame_seq: self.seq,
            monitor_index,
            codec: self.codec.to_proto_i32(),
            pixel_format: 1, // YUV420P
            keyframe: self.is_keyframe,
            encoded_data: self.data,
            nonce: self.nonce.to_vec(),
            width: self.width,
            height: self.height,
            pts_ms: self.pts_ms,
            encode_duration_us: self.encode_duration_us,
            damage_hint: vec![],
        }
    }
}

/// Pilote le pipeline d'encodage dans une tâche tokio
/// Encodeur actif selon la feature de compilation
#[cfg(feature = "openh264")]
pub use openh264_enc::Openh264Encoder as ActiveEncoder;
#[cfg(not(feature = "openh264"))]
pub use ffmpeg::FfmpegEncoder as ActiveEncoder;

pub struct EncoderPipeline {
    params: EncoderParams,
    session_key: Option<SessionKey>,
}

impl EncoderPipeline {
    /// Crée un nouveau pipeline d'encodage
    pub fn new(params: EncoderParams, session_key: Option<SessionKey>) -> Self {
        Self { params, session_key }
    }

    /// Démarre le pipeline en arrière-plan.
    ///
    /// Consomme les `RawFrame` sur `rx_raw`, produit les `EncodedFrame` sur `tx_encoded`.
    pub fn start(
        self,
        mut rx_raw: mpsc::Receiver<RawFrame>,
        tx_encoded: mpsc::Sender<EncodedFrame>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        // L'encodage FFmpeg est bloquant → thread dédié
        tokio::task::spawn_blocking(move || {
            let mut encoder = ActiveEncoder::new(&self.params)
                .context("initialisation encodeur")?;

            let mut cipher = self.session_key.as_ref().map(StreamCipher::new);
            let session_start_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64;

            info!(codec = ?self.params.codec, hw = self.params.hardware_accel, "encodeur démarré");

            loop {
                if shutdown.is_cancelled() {
                    info!("encodeur arrêté sur signal");
                    break;
                }

                // Réception bloquante avec timeout
                let raw_frame = match rx_raw.blocking_recv() {
                    Some(f) => f,
                    None => {
                        info!("channel de capture fermé, arrêt encodeur");
                        break;
                    }
                };

                let encode_start = std::time::Instant::now();

                // Forcer un IDR si la frame source est un keyframe
                if raw_frame.is_keyframe {
                    encoder.request_keyframe();
                }

                // Encodage
                let nal_data = match encoder.encode_frame(&raw_frame) {
                    Ok(data) => data,
                    Err(e) => {
                        warn!(error = %e, seq = raw_frame.seq, "erreur encodage frame — ignorée");
                        continue;
                    }
                };

                // Frames vides (bufferisation encodeur) → skip
                if nal_data.is_empty() {
                    continue;
                }

                let encode_duration_us = encode_start.elapsed().as_micros() as u32;
                let pts_ms = ((raw_frame.timestamp_us.saturating_sub(session_start_us)) / 1000) as u32;

                // Chiffrement E2E si clé de session disponible
                let (final_data, nonce) = if let Some(ref mut c) = cipher {
                    match c.encrypt(&nal_data) {
                        Ok((ciphertext, nonce)) => (ciphertext, nonce),
                        Err(e) => {
                            warn!(error = %e, "erreur chiffrement — frame ignorée");
                            continue;
                        }
                    }
                } else {
                    // Pas de chiffrement E2E (mode dev/test)
                    let nonce = [0u8; 12];
                    (nal_data, nonce)
                };

                let encoded = EncodedFrame {
                    data: final_data,
                    nonce,
                    seq: raw_frame.seq,
                    pts_ms,
                    codec: self.params.codec,
                    is_keyframe: raw_frame.is_keyframe,
                    width: raw_frame.width,
                    height: raw_frame.height,
                    encode_duration_us,
                };

                debug!(
                    seq    = encoded.seq,
                    kf     = encoded.is_keyframe,
                    size   = encoded.data.len(),
                    enc_us = encode_duration_us,
                    "frame encodée"
                );

                if tx_encoded.blocking_send(encoded).is_err() {
                    info!("channel de sortie fermé, arrêt encodeur");
                    break;
                }
            }

            Ok(())
        })
    }
}
