//! Décodeur FFmpeg pour NIDAN.
//!
//! Gère H.264, H.265 et AV1 en software et hardware (VAAPI/VDPAU/DXVA2).
//! Produit des frames BGRA prêtes pour SDL2/wgpu.

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::DecoderCodec;

/// Décodeur FFmpeg
pub struct FfmpegDecoder {
    codec: DecoderCodec,
    width: u32,
    height: u32,
    hardware: bool,
    inner: DecoderInner,
    /// Compteur de frames pour stats
    frame_count: u64,
    /// Demande de resync IDR
    need_idr: bool,
}

enum DecoderInner {
    #[cfg(all(feature = "ffmpeg", not(feature = "stub")))]
    Ffmpeg(FfmpegContext),
    Stub { width: u32, height: u32 },
}

#[cfg(all(feature = "ffmpeg", not(feature = "stub")))]
struct FfmpegContext {
    _codec_name: String,
    // En production : AVCodecContext*, AVFrame*, AVPacket*, SwsContext*
    // Phase 2.1 : implémentation complète avec ffmpeg-sys-next
}

impl FfmpegDecoder {
    /// Crée un décodeur FFmpeg réel
    pub fn new(codec: DecoderCodec, hardware: bool) -> Result<Self> {
        #[cfg(all(feature = "ffmpeg", not(feature = "stub")))]
        {
            // TODO Phase 2.1 : avcodec_find_decoder → avcodec_alloc_context3
            // → avcodec_open2 → sws_getContext (YUV420P → BGRA)
            // Pour l'instant fallback stub
            warn!("décodeur FFmpeg réel non encore implémenté — stub actif");
        }

        Ok(Self {
            codec,
            width: 1920,
            height: 1080,
            hardware,
            inner: DecoderInner::Stub { width: 1920, height: 1080 },
            frame_count: 0,
            need_idr: false,
        })
    }

    /// Crée un décodeur stub avec dimensions connues
    pub fn stub(width: u32, height: u32) -> Self {
        Self {
            codec: DecoderCodec::H264,
            width,
            height,
            hardware: false,
            inner: DecoderInner::Stub { width, height },
            frame_count: 0,
            need_idr: false,
        }
    }

    /// Demande un IDR frame pour resync après erreur
    pub fn request_idr(&mut self) {
        self.need_idr = true;
    }

    /// Décode un paquet NAL.
    ///
    /// Retourne `Ok(Some(pixels))` si une frame est disponible,
    /// `Ok(None)` si la frame est bufferisée (B-frames),
    /// `Err` sur erreur fatale.
    pub fn decode_packet(&mut self, nal: &[u8], is_keyframe: bool) -> Result<Option<Vec<u8>>> {
        // Si on attend un IDR et ce n'est pas un keyframe → skip
        if self.need_idr && !is_keyframe {
            return Ok(None);
        }
        if is_keyframe {
            self.need_idr = false;
        }

        let pixels = match &self.inner {
            #[cfg(all(feature = "ffmpeg", not(feature = "stub")))]
            DecoderInner::Ffmpeg(_ctx) => {
                // TODO Phase 2.1 :
                // 1. av_packet_from_data(nal)
                // 2. avcodec_send_packet
                // 3. avcodec_receive_frame (loop)
                // 4. sws_scale : YUV420P → BGRA
                // 5. retourner les pixels BGRA
                Self::decode_stub(self.width, self.height, self.frame_count)
            }
            DecoderInner::Stub { width, height } => {
                Self::decode_stub(*width, *height, self.frame_count)
            }
        };

        self.frame_count += 1;
        Ok(Some(pixels))
    }

    /// Décode stub : produit un frame BGRA synthétique
    fn decode_stub(width: u32, height: u32, frame_count: u64) -> Vec<u8> {
        let size = (width * height * 4) as usize;
        let mut pixels = vec![0u8; size];

        // Dégradé animé pour visualiser le flux
        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                // BGRA
                pixels[idx]     = ((x + frame_count as u32 * 2) % 256) as u8; // B
                pixels[idx + 1] = ((y + frame_count as u32) % 256) as u8;     // G
                pixels[idx + 2] = (frame_count as u32 % 256) as u8;           // R
                pixels[idx + 3] = 255;                                         // A
            }
        }

        // Texte synthétique : barre de statut en haut
        // (simple rectangle blanc pour indiquer que le stream est actif)
        for x in 0..width.min(400) {
            let idx = (x * 4) as usize;
            pixels[idx]     = 255; // B
            pixels[idx + 1] = 255; // G
            pixels[idx + 2] = 255; // R
            pixels[idx + 3] = 255; // A
        }

        pixels
    }

    /// Flush le décodeur (fin de stream)
    pub fn flush(&mut self) -> Result<Vec<Vec<u8>>> {
        // TODO Phase 2.1 : drain B-frames via avcodec_send_packet(NULL)
        Ok(vec![])
    }

    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }
}
