//! Pipeline de décodage vidéo.
//!
//! Reçoit des `VideoFrame` proto chiffrés, déchiffre avec ChaCha20-Poly1305,
//! décode via FFmpeg (H.264/H.265/AV1) et produit des `DecodedFrame` BGRA
//! prêtes pour le rendu SDL2/wgpu.
//!
//! ## Pipeline
//! ```text
//! [VideoFrame proto chiffré]
//!        ↓
//! [Déchiffrement ChaCha20-Poly1305]
//!        ↓
//! [Décodage FFmpeg : NAL → YUV420P]
//!     ↓ hardware            ↓ software
//! [VDPAU/VAAPI/DXVA2]   [libavcodec]
//!        ↓
//! [Conversion YUV420P → BGRA (sws_scale)]
//!        ↓
//! [DecodedFrame → channel renderer]
//! ```

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use nidan_common::crypto::StreamCipher;
use nidan_proto::v1::{VideoCodec, VideoFrame};

pub mod ffmpeg;

/// Une frame décodée prête pour l'affichage
#[derive(Debug)]
pub struct DecodedFrame {
    /// Pixels BGRA (4 bytes par pixel)
    pub data: Vec<u8>,
    /// Dimensions
    pub width: u32,
    pub height: u32,
    /// Numéro de séquence (pour métriques et reordering)
    pub seq: u64,
    /// Timestamp présentation (ms)
    pub pts_ms: u32,
    /// Indice du moniteur source
    pub monitor_index: u32,
    /// Durée de décodage (µs)
    pub decode_duration_us: u32,
}

/// Codec de décodage
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DecoderCodec {
    H264,
    H265,
    Av1,
}

impl DecoderCodec {
    pub fn from_proto(codec: i32) -> Self {
        match codec {
            1 => Self::H264,
            2 => Self::H265,
            3 => Self::Av1,
            _ => Self::H264,
        }
    }

    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "hevc",
            Self::Av1  => "av1",
        }
    }

    pub fn ffmpeg_hw_name(self) -> Option<&'static str> {
        match self {
            Self::H264 => Some("h264_vaapi"),
            Self::H265 => Some("hevc_vaapi"),
            Self::Av1  => None,
        }
    }
}

/// Pipeline de décodage complet
pub struct DecoderPipeline {
    hardware_decode: bool,
    cipher: Option<StreamCipher>,
}

impl DecoderPipeline {
    pub fn new(hardware_decode: bool, cipher: Option<StreamCipher>) -> Self {
        Self { hardware_decode, cipher }
    }

    /// Démarre le pipeline en arrière-plan.
    /// Consomme `rx_frames` (VideoFrame proto), produit `tx_decoded` (DecodedFrame).
    pub fn start(
        self,
        mut rx_frames: mpsc::Receiver<VideoFrame>,
        tx_decoded: mpsc::Sender<DecodedFrame>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        // Le décodage FFmpeg est bloquant → spawn_blocking
        tokio::task::spawn_blocking(move || {
            let mut decoder: Option<ffmpeg::FfmpegDecoder> = None;
            let mut cipher = self.cipher;
            let mut frames_decoded = 0u64;
            let mut frames_dropped = 0u64;

            info!(hw = self.hardware_decode, "décodeur démarré");

            loop {
                if shutdown.is_cancelled() {
                    info!("décodeur arrêté sur signal");
                    break;
                }

                let frame = match rx_frames.blocking_recv() {
                    Some(f) => f,
                    None => {
                        info!("channel de frames fermé, arrêt décodeur");
                        break;
                    }
                };

                let t_start = std::time::Instant::now();

                // Étape 1 : déchiffrement E2E
                let nal_data = if let Some(ref mut c) = cipher {
                    match c.decrypt(&frame.encoded_data, &frame.nonce) {
                        Ok(data) => data,
                        Err(e) => {
                            warn!(error = %e, seq = frame.frame_seq, "déchiffrement échoué — frame ignorée");
                            frames_dropped += 1;
                            continue;
                        }
                    }
                } else {
                    frame.encoded_data.clone()
                };

                // Étape 2 : initialisation du décodeur au premier paquet
                // (on connaît alors le codec depuis le proto)
                if decoder.is_none() {
                    let codec = DecoderCodec::from_proto(frame.codec);
                    info!(codec = codec.ffmpeg_name(), "initialisation décodeur FFmpeg");
                    match ffmpeg::FfmpegDecoder::new(codec, self.hardware_decode) {
                        Ok(d) => decoder = Some(d),
                        Err(e) => {
                            warn!(error = %e, "décodeur FFmpeg indispo — fallback stub");
                            decoder = Some(ffmpeg::FfmpegDecoder::stub(
                                frame.width, frame.height
                            ));
                        }
                    }
                }

                // Étape 3 : décodage
                let dec = decoder.as_mut().unwrap();
                let pixels = match dec.decode_packet(&nal_data, frame.keyframe) {
                    Ok(Some(px)) => px,
                    Ok(None) => {
                        // Frame bufferisée par le décodeur — normal
                        debug!(seq = frame.frame_seq, "frame bufferisée");
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, seq = frame.frame_seq, "décodage échoué — frame ignorée");
                        frames_dropped += 1;
                        // Forcer un IDR pour resync
                        dec.request_idr();
                        continue;
                    }
                };

                let decode_duration_us = t_start.elapsed().as_micros() as u32;

                let decoded = DecodedFrame {
                    data: pixels,
                    width: frame.width,
                    height: frame.height,
                    seq: frame.frame_seq,
                    pts_ms: frame.pts_ms,
                    monitor_index: frame.monitor_index,
                    decode_duration_us,
                };

                debug!(
                    seq    = decoded.seq,
                    dec_us = decode_duration_us,
                    "frame décodée"
                );

                if tx_decoded.blocking_send(decoded).is_err() {
                    info!("channel renderer fermé, arrêt décodeur");
                    break;
                }

                frames_decoded += 1;

                // Log métriques périodiques
                if frames_decoded % 300 == 0 {
                    info!(
                        decoded = frames_decoded,
                        dropped = frames_dropped,
                        "métriques décodeur"
                    );
                }
            }

            info!(frames_decoded, frames_dropped, "décodeur terminé");
            Ok(())
        })
    }
}
