//! Rendu vidéo via SDL2.
//!
//! Consomme les `DecodedFrame` BGRA depuis le pipeline de décodage
//! et les affiche dans une fenêtre SDL2 avec support :
//! - Mode fenêtré et plein écran
//! - Scaling adaptatif (fit / stretch / 1:1)
//! - Mode seamless (overlay transparent)
//! - Overlay HUD (métriques, statut connexion)
//! - Gestion des événements fenêtre (resize, focus, iconify)

use anyhow::{Context, Result};
use std::sync::mpsc;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, info, warn};

use crate::decoder::DecodedFrame;
use crate::input::InputEvent;
use crate::config::DisplayConfig;

pub mod sdl;

/// Mode de scaling de l'image
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScalingMode {
    /// Ajuste l'image en conservant le ratio (letterbox)
    Fit,
    /// Étire l'image pour remplir la fenêtre
    Stretch,
    /// Affichage pixel-perfect 1:1
    OneToOne,
}

impl ScalingMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "stretch"  => Self::Stretch,
            "1:1"      => Self::OneToOne,
            _          => Self::Fit,
        }
    }
}

/// Rectangle de destination pour le rendu (après scaling)
#[derive(Debug, Clone, Copy)]
pub struct RenderRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl RenderRect {
    /// Calcule le rectangle de destination selon le mode de scaling
    pub fn compute(
        src_w: u32, src_h: u32,
        dst_w: u32, dst_h: u32,
        mode: ScalingMode,
    ) -> Self {
        match mode {
            ScalingMode::Stretch => Self { x: 0, y: 0, w: dst_w, h: dst_h },
            ScalingMode::OneToOne => {
                let x = ((dst_w as i32) - (src_w as i32)) / 2;
                let y = ((dst_h as i32) - (src_h as i32)) / 2;
                Self { x, y, w: src_w, h: src_h }
            }
            ScalingMode::Fit => {
                let ratio_w = dst_w as f32 / src_w as f32;
                let ratio_h = dst_h as f32 / src_h as f32;
                let ratio   = ratio_w.min(ratio_h);
                let w = (src_w as f32 * ratio) as u32;
                let h = (src_h as f32 * ratio) as u32;
                let x = ((dst_w - w) / 2) as i32;
                let y = ((dst_h - h) / 2) as i32;
                Self { x, y, w, h }
            }
        }
    }

    /// Convertit des coordonnées fenêtre en coordonnées normalisées [0.0, 1.0]
    pub fn window_to_normalized(&self, wx: i32, wy: i32) -> Option<(f32, f32)> {
        let rx = wx - self.x;
        let ry = wy - self.y;
        if rx < 0 || ry < 0 || rx >= self.w as i32 || ry >= self.h as i32 {
            return None; // Hors de la zone de rendu
        }
        Some((
            rx as f32 / self.w as f32,
            ry as f32 / self.h as f32,
        ))
    }
}

/// Métriques d'affichage pour le HUD
#[derive(Debug, Default, Clone)]
pub struct RenderMetrics {
    pub fps: f32,
    pub frames_rendered: u64,
    pub frames_dropped: u64,
    pub decode_latency_us: u32,
    pub last_frame_size: u32,
    pub connection_status: ConnectionStatus,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum ConnectionStatus {
    #[default]
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
}

impl ConnectionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting    => "Connexion...",
            Self::Connected     => "Connecté",
            Self::Reconnecting  => "Reconnexion...",
            Self::Disconnected  => "Déconnecté",
        }
    }
}

/// Handle partagé vers le renderer — permet de mettre à jour les métriques
/// depuis d'autres tâches (ex: le pipeline de décodage)
#[derive(Clone)]
pub struct RendererHandle {
    pub metrics_tx: tokio::sync::watch::Sender<RenderMetrics>,
}

/// Démarre le renderer SDL2 dans le thread principal (obligatoire pour SDL2).
///
/// Retourne un channel pour envoyer les frames décodées,
/// un channel pour recevoir les InputEvent générés par SDL2,
/// et un handle vers les métriques.
pub fn start_renderer(
    config: DisplayConfig,
    initial_width: u32,
    initial_height: u32,
) -> Result<(
    mpsc::SyncSender<DecodedFrame>,    // frames vers renderer
    tokio_mpsc::Receiver<InputEvent>,   // inputs depuis renderer
    tokio::sync::watch::Receiver<RenderMetrics>, // métriques
    std::thread::JoinHandle<Result<()>>,
)> {
    let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedFrame>(4);
    let (input_tx, input_rx) = tokio_mpsc::channel::<InputEvent>(256);
    let (metrics_tx, metrics_rx) = tokio::sync::watch::channel(RenderMetrics::default());

    let thread = std::thread::spawn(move || {
        sdl::run_sdl2_loop(config, initial_width, initial_height, frame_rx, input_tx, metrics_tx)
    });

    Ok((frame_tx, input_rx, metrics_rx, thread))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_rect_fit() {
        // Image 1920x1080 dans fenêtre 800x600
        let r = RenderRect::compute(1920, 1080, 800, 600, ScalingMode::Fit);
        // Ratio : 800/1920 = 0.4167, 600/1080 = 0.5556 → min = 0.4167
        // w = 1920 * 0.4167 = 800, h = 1080 * 0.4167 = 450
        assert_eq!(r.w, 800);
        assert_eq!(r.h, 450);
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 75); // (600-450)/2
    }

    #[test]
    fn test_render_rect_stretch() {
        let r = RenderRect::compute(1920, 1080, 800, 600, ScalingMode::Stretch);
        assert_eq!(r.w, 800);
        assert_eq!(r.h, 600);
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
    }

    #[test]
    fn test_window_to_normalized_inside() {
        let r = RenderRect { x: 0, y: 75, w: 800, h: 450 };
        let (nx, ny) = r.window_to_normalized(400, 300).unwrap();
        assert!((nx - 0.5).abs() < 0.01);
        assert!((ny - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_window_to_normalized_outside() {
        let r = RenderRect { x: 0, y: 75, w: 800, h: 450 };
        assert!(r.window_to_normalized(400, 10).is_none()); // hors zone
    }
}
