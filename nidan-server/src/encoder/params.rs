//! Paramètres de configuration de l'encodeur FFmpeg.

use anyhow::{bail, Result};

use crate::encoder::CodecChoice;

/// Paramètres complets de l'encodeur
#[derive(Debug, Clone)]
pub struct EncoderParams {
    /// Codec sélectionné
    pub codec: CodecChoice,
    /// Largeur en pixels
    pub width: u32,
    /// Hauteur en pixels
    pub height: u32,
    /// FPS cible
    pub fps: u32,
    /// Bitrate en kbps (0 = CRF/qualité constante)
    pub bitrate_kbps: u32,
    /// Qualité CRF (0–51 pour H.264, lower = better)
    pub crf: u32,
    /// Profil H.264 : "baseline", "main", "high"
    pub h264_profile: String,
    /// Preset d'encodage : "ultrafast".."veryslow"
    pub preset: String,
    /// Utiliser l'accélération matérielle
    pub hardware_accel: bool,
    /// Device hardware (ex: "/dev/dri/renderD128" pour VAAPI)
    pub hw_device: Option<String>,
    /// Intervalle entre keyframes (secondes)
    pub keyframe_interval_secs: u32,
    /// Threads d'encodage (0 = auto)
    pub threads: u32,
}

impl EncoderParams {
    /// Crée des paramètres optimisés pour la latence (bureau distant)
    pub fn for_remote_desktop(
        codec: CodecChoice,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Self {
        Self {
            codec,
            width,
            height,
            fps,
            bitrate_kbps: 0,     // CRF
            crf: 28,             // Qualité raisonnable
            h264_profile: "high".to_string(),
            preset: "ultrafast".to_string(), // Priorité latence
            hardware_accel: true,
            hw_device: None,
            keyframe_interval_secs: 2,
            threads: 0,
        }
    }

    /// Valide la cohérence des paramètres
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            bail!("résolution invalide: {}x{}", self.width, self.height);
        }
        if self.fps == 0 || self.fps > 120 {
            bail!("fps invalide: {}", self.fps);
        }
        match self.preset.as_str() {
            "ultrafast" | "superfast" | "veryfast" | "faster" | "fast"
            | "medium" | "slow" | "slower" | "veryslow" => {}
            other => bail!("preset inconnu: {other}"),
        }
        Ok(())
    }

    /// Retourne le nom du codec FFmpeg correspondant
    pub fn ffmpeg_codec_name(&self) -> &'static str {
        match self.codec {
            CodecChoice::H264 => "libx264",
            CodecChoice::H265 => "libx265",
            CodecChoice::Av1  => "libaom-av1",
        }
    }

    /// Retourne le nom du codec hardware FFmpeg
    pub fn ffmpeg_hw_codec_name(&self) -> Option<&'static str> {
        match self.codec {
            CodecChoice::H264 => Some("h264_vaapi"),
            CodecChoice::H265 => Some("hevc_vaapi"),
            CodecChoice::Av1  => None, // Pas encore de VAAPI AV1 encoder stable
        }
    }
}

impl Default for EncoderParams {
    fn default() -> Self {
        Self::for_remote_desktop(CodecChoice::H264, 1920, 1080, 30)
    }
}
