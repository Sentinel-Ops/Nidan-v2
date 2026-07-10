//! Encodeur H.264 réel via openh264 (Cisco), derrière la feature `openh264`.
//!
//! Convertit les frames BGRA capturées en H.264 Annex B.
//! Pas de dépendance système : openh264 compile son code C bundlé.

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::capture::RawFrame;
use super::params::EncoderParams;

#[cfg(feature = "openh264")]
use openh264::encoder::Encoder;
#[cfg(feature = "openh264")]
use openh264::formats::{RgbSliceU8, YUVBuffer};

/// Encodeur H.264 réel
pub struct Openh264Encoder {
    #[cfg(feature = "openh264")]
    encoder: Encoder,
    width: u32,
    height: u32,
    frame_count: u64,
    force_keyframe: bool,
    keyframe_interval: u64,
}

impl Openh264Encoder {
    /// Crée un encodeur H.264 openh264
    pub fn new(params: &EncoderParams) -> Result<Self> {
        params.validate().context("paramètres encodeur invalides")?;

        #[cfg(feature = "openh264")]
        {
            let encoder = Encoder::new()
                .context("création encodeur openh264")?;
            info!(
                width = params.width,
                height = params.height,
                fps = params.fps,
                "encodeur H.264 openh264 initialisé"
            );
            Ok(Self {
                encoder,
                width: params.width,
                height: params.height,
                frame_count: 0,
                force_keyframe: true,
                keyframe_interval: (params.fps as u64 * params.keyframe_interval_secs as u64).max(1),
            })
        }

        #[cfg(not(feature = "openh264"))]
        {
            warn!("Openh264Encoder appelé sans la feature openh264 — utilisez le stub");
            Ok(Self {
                width: params.width,
                height: params.height,
                frame_count: 0,
                force_keyframe: true,
                keyframe_interval: 60,
            })
        }
    }

    /// Demande une keyframe (IDR) à la prochaine frame
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Encode une frame BGRA en H.264 (NAL units Annex B).
    /// Retourne les bytes encodés (vide si pas de sortie).
    pub fn encode_frame(&mut self, frame: &RawFrame) -> Result<Vec<u8>> {
        #[cfg(feature = "openh264")]
        {
            // Forcer un IDR si demandé ou selon l'intervalle
            if self.force_keyframe
                || self.frame_count % self.keyframe_interval == 0
            {
                self.encoder.force_intra_frame();
                self.force_keyframe = false;
            }

            // Conversion BGRA → RGB (openh264 attend du RGB)
            let rgb = bgra_to_rgb(&frame.data, self.width, self.height);
            let rgb_source = RgbSliceU8::new(
                &rgb,
                (self.width as usize, self.height as usize),
            );
            let yuv = YUVBuffer::from_rgb_source(rgb_source);

            let bitstream = self.encoder.encode(&yuv)
                .context("encodage openh264")?;

            self.frame_count += 1;
            Ok(bitstream.to_vec())
        }

        #[cfg(not(feature = "openh264"))]
        {
            let _ = frame;
            self.frame_count += 1;
            Ok(vec![])
        }
    }

    pub fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Convertit BGRA (capture X11) → RGB (openh264)
#[cfg(feature = "openh264")]
fn bgra_to_rgb(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgb = Vec::with_capacity(n * 3);
    let mut i = 0;
    while i + 3 < bgra.len() && rgb.len() < n * 3 {
        rgb.push(bgra[i]);     // R (PipeWire livre RGBA malgré négociation BGRA)
        rgb.push(bgra[i + 1]); // G
        rgb.push(bgra[i + 2]); // B (PipeWire livre RGBA malgré négociation BGRA)
        i += 4;
    }
    // Compléter si nécessaire
    while rgb.len() < n * 3 {
        rgb.push(0);
    }
    rgb
}
