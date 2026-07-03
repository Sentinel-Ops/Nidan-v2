//! Wrapper autour de l'API FFmpeg pour l'encodage vidéo.
//!
//! En mode `stub` (pas de FFmpeg disponible), simule un encodeur
//! qui produit des données synthétiques pour les tests.

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::capture::RawFrame;
use super::params::EncoderParams;

/// Encodeur FFmpeg
pub struct FfmpegEncoder {
    params: EncoderParams,
    /// Compteur de frames pour gérer les keyframes forcées
    frame_count: u64,
    /// Keyframe forcée à la prochaine frame
    force_keyframe: bool,
    /// Handle interne (opaque selon la feature)
    inner: EncoderInner,
}

/// État interne selon la disponibilité de FFmpeg
enum EncoderInner {
    #[cfg(all(feature = "ffmpeg", not(feature = "stub")))]
    Ffmpeg(FfmpegContext),
    Stub,
}

#[cfg(all(feature = "ffmpeg", not(feature = "stub")))]
struct FfmpegContext {
    // En production : codec_ctx, frame, packet, sws_ctx
    // Les types exacts viennent de ffmpeg-sys-next
    // Pour la Phase 1 : placeholder typé mais non initialisé
    _codec_name: String,
    _width: u32,
    _height: u32,
}

impl FfmpegEncoder {
    /// Crée et initialise un encodeur FFmpeg
    pub fn new(params: &EncoderParams) -> Result<Self> {
        params.validate().context("paramètres encodeur invalides")?;

        let inner = Self::init_encoder(params)?;

        info!(
            codec  = params.ffmpeg_codec_name(),
            width  = params.width,
            height = params.height,
            fps    = params.fps,
            preset = %params.preset,
            hw     = params.hardware_accel,
            "encodeur FFmpeg initialisé"
        );

        Ok(Self {
            params: params.clone(),
            frame_count: 0,
            force_keyframe: true, // Première frame toujours IDR
            inner,
        })
    }

    fn init_encoder(params: &EncoderParams) -> Result<EncoderInner> {
        #[cfg(all(feature = "ffmpeg", not(feature = "stub")))]
        {
            // TODO Phase 1.1 : initialisation complète avec ffmpeg-sys-next
            // avcodec_find_encoder → avcodec_alloc_context3 → avcodec_open2
            // sws_getContext pour conversion BGRA → YUV420P
            warn!("encodeur FFmpeg réel non encore implémenté — fallback stub");
            return Ok(EncoderInner::Stub);
        }

        #[allow(unreachable_code)]
        {
            info!("encodeur en mode stub (pas de FFmpeg)");
            Ok(EncoderInner::Stub)
        }
    }

    /// Demande une keyframe (IDR) à la prochaine encode
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Encode une frame brute en NAL units.
    ///
    /// Retourne un Vec vide si l'encodeur bufferise la frame (normal en début).
    pub fn encode_frame(&mut self, frame: &RawFrame) -> Result<Vec<u8>> {
        let is_keyframe = self.force_keyframe
            || self.frame_count == 0
            || self.frame_count % (self.params.fps as u64 * self.params.keyframe_interval_secs as u64) == 0;

        self.force_keyframe = false;

        let result = match &self.inner {
            #[cfg(all(feature = "ffmpeg", not(feature = "stub")))]
            EncoderInner::Ffmpeg(_ctx) => {
                // TODO Phase 1.1 : encodage réel
                // 1. sws_scale : BGRA → YUV420P
                // 2. av_frame_make_writable
                // 3. avcodec_send_frame
                // 4. avcodec_receive_packet (loop)
                // 5. Concaténer les NAL units
                self.encode_stub(frame, is_keyframe)
            }
            EncoderInner::Stub => {
                self.encode_stub(frame, is_keyframe)
            }
        };

        self.frame_count += 1;
        result
    }

    /// Encodeur stub : produit un faux NAL header + hash de la frame
    fn encode_stub(&self, frame: &RawFrame, is_keyframe: bool) -> Result<Vec<u8>> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // Simule un délai d'encodage réaliste (~1–3 ms pour ultrafast)
        // En mode stub on ne dort pas pour ne pas bloquer les tests

        // NAL header synthétique
        // Structure : [4 bytes start code] [1 byte NAL type] [payload]
        let mut nal = Vec::with_capacity(64);

        // Start code Annex B : 0x00 0x00 0x00 0x01
        nal.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);

        // NAL unit type : 5 = IDR, 1 = non-IDR (H.264)
        let nal_type: u8 = if is_keyframe { 0x65 } else { 0x41 };
        nal.push(nal_type);

        // Métadonnées stub : seq + dims + hash pixels
        nal.extend_from_slice(&frame.seq.to_le_bytes());
        nal.extend_from_slice(&frame.width.to_le_bytes());
        nal.extend_from_slice(&frame.height.to_le_bytes());

        // Hash des pixels pour détecter les changements en test
        let mut hasher = DefaultHasher::new();
        // On ne hash que les 1024 premiers bytes pour la perf
        let sample = &frame.data[..frame.data.len().min(1024)];
        sample.hash(&mut hasher);
        nal.extend_from_slice(&hasher.finish().to_le_bytes());

        // Payload synthétique variable (simule la compression)
        // Keyframe : ~20KB, delta frame : ~2KB
        let payload_size = if is_keyframe { 20_000 } else { 2_000 };
        nal.extend(std::iter::repeat(0x42u8).take(payload_size));

        Ok(nal)
    }

    /// Flush l'encodeur (fin de session) — retourne les frames restantes
    pub fn flush(&mut self) -> Result<Vec<Vec<u8>>> {
        // TODO Phase 1.1 : avcodec_send_frame(NULL) + drain avcodec_receive_packet
        Ok(vec![])
    }

    /// Modifie le bitrate à chaud (pour adaptation débit)
    pub fn set_bitrate(&mut self, kbps: u32) {
        info!(kbps, "changement de bitrate");
        self.params.bitrate_kbps = kbps;
        // TODO Phase 1.1 : AVCodecContext.rc_max_rate via avcodec_parameters_from_context
    }

    /// Résolution actuelle de l'encodeur
    pub fn resolution(&self) -> (u32, u32) {
        (self.params.width, self.params.height)
    }
}
