//! Décodeur H.264 réel via openh264 (Cisco), derrière la feature `openh264`.
//!
//! Miroir de l'encodeur serveur : transforme un flux H.264 Annex B
//! en frames BGRA prêtes pour le rendu. Pas de dépendance système.

use anyhow::{Context, Result};
use tracing::{debug, info};

use super::DecoderCodec;

#[cfg(feature = "openh264")]
use openh264::decoder::Decoder;
#[cfg(feature = "openh264")]
use openh264::formats::YUVSource;

/// Décodeur H.264 openh264
pub struct Openh264Decoder {
    #[cfg(feature = "openh264")]
    decoder: Decoder,
    width: u32,
    height: u32,
    frames_decoded: u64,
    is_stub: bool,
}

impl Openh264Decoder {
    /// Crée un décodeur H.264 réel
    pub fn new(codec: DecoderCodec, _hardware: bool) -> Result<Self> {
        if codec != DecoderCodec::H264 {
            anyhow::bail!("openh264 ne décode que le H.264 (reçu {:?})", codec);
        }

        #[cfg(feature = "openh264")]
        {
            let decoder = Decoder::new()
                .context("création décodeur openh264")?;
            info!("décodeur H.264 openh264 initialisé");
            Ok(Self {
                decoder,
                width: 0,
                height: 0,
                frames_decoded: 0,
                is_stub: false,
            })
        }

        #[cfg(not(feature = "openh264"))]
        {
            anyhow::bail!("feature openh264 désactivée — utilisez stub()")
        }
    }

    /// Décodeur stub (frames grises) pour build sans la feature
    pub fn stub(width: u32, height: u32) -> Self {
        Self {
            #[cfg(feature = "openh264")]
            decoder: Decoder::new().expect("decoder stub"),
            width,
            height,
            frames_decoded: 0,
            is_stub: true,
        }
    }

    /// Demande une resynchronisation (no-op pour openh264 : géré en interne)
    pub fn request_idr(&mut self) {
        debug!("resync IDR demandée");
    }

    /// Décode un paquet H.264 (NAL Annex B) → BGRA, ou None si bufferisé.
    pub fn decode_packet(&mut self, nal: &[u8], _is_keyframe: bool) -> Result<Option<Vec<u8>>> {
        if self.is_stub {
            // Frame grise unie pour le mode stub
            let (w, h) = (self.width.max(1), self.height.max(1));
            let mut bgra = vec![0u8; (w * h * 4) as usize];
            for px in bgra.chunks_mut(4) {
                px[0] = 64; px[1] = 64; px[2] = 64; px[3] = 255;
            }
            self.frames_decoded += 1;
            return Ok(Some(bgra));
        }

        #[cfg(feature = "openh264")]
        {
            match self.decoder.decode(nal).context("décodage openh264")? {
                Some(yuv) => {
                    let (w, h) = yuv.dimensions();
                    self.width = w as u32;
                    self.height = h as u32;

                    // openh264 écrit du RGBA → on convertit en BGRA pour le renderer
                    let mut rgba = vec![0u8; w * h * 4];
                    yuv.write_rgba8(&mut rgba);

                    // RGBA → BGRA (swap R/B)
                    let mut bgra = rgba;
                    let mut i = 0;
                    while i + 2 < bgra.len() {
                        bgra.swap(i, i + 2);
                        i += 4;
                    }

                    self.frames_decoded += 1;
                    debug!(seq = self.frames_decoded, w, h, "frame H.264 décodée");
                    Ok(Some(bgra))
                }
                None => Ok(None), // frame bufferisée (besoin de plus de données)
            }
        }

        #[cfg(not(feature = "openh264"))]
        {
            let _ = nal;
            Ok(None)
        }
    }

    /// Vide les frames restantes du décodeur
    pub fn flush(&mut self) -> Result<Vec<Vec<u8>>> {
        Ok(vec![])
    }

    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }
}
