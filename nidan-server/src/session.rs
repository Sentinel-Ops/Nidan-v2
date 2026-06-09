//! État d'une session côté serveur.
//!
//! Maintient le contexte complet d'une connexion active :
//! identité, clés de session, métriques temps réel.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, Mutex};
use tracing::info;

use nidan_common::crypto::SessionKeys;
use nidan_common::session::SessionId;
use nidan_proto::v1::{EncodedFrame, SessionState};

use crate::encoder::EncodedFrame as LocalEncodedFrame;

/// État d'une session côté serveur
pub struct ServerSession {
    /// Identifiant de session
    pub id: SessionId,
    /// État courant
    pub state: Arc<Mutex<SessionState>>,
    /// Clés de session dérivées
    pub keys: Option<SessionKeys>,
    /// Instant de début de session
    pub started_at: Instant,
    /// Métriques de session
    pub metrics: Arc<Mutex<SessionMetrics>>,
}

/// Métriques temps réel de la session
#[derive(Debug, Default)]
pub struct SessionMetrics {
    pub frames_encoded: u64,
    pub frames_sent: u64,
    pub frames_dropped: u64,
    pub bytes_sent: u64,
    pub last_rtt_us: u64,
    pub avg_encode_us: f64,
}

impl SessionMetrics {
    pub fn record_frame(&mut self, size: usize, encode_us: u32) {
        self.frames_encoded += 1;
        self.bytes_sent += size as u64;
        // Moyenne mobile exponentielle (α = 0.1)
        self.avg_encode_us = 0.9 * self.avg_encode_us + 0.1 * encode_us as f64;
    }

    pub fn fps_actual(&self, elapsed_secs: f64) -> f32 {
        if elapsed_secs > 0.0 {
            (self.frames_sent as f64 / elapsed_secs) as f32
        } else {
            0.0
        }
    }

    pub fn bitrate_kbps(&self, elapsed_secs: f64) -> u32 {
        if elapsed_secs > 0.0 {
            ((self.bytes_sent as f64 * 8.0) / (elapsed_secs * 1000.0)) as u32
        } else {
            0
        }
    }
}

impl ServerSession {
    pub fn new(id: SessionId, keys: Option<SessionKeys>) -> Self {
        info!(session_id = %id, "nouvelle session serveur créée");
        Self {
            id,
            state: Arc::new(Mutex::new(SessionState::SessionStateInit)),
            keys,
            started_at: Instant::now(),
            metrics: Arc::new(Mutex::new(SessionMetrics::default())),
        }
    }

    pub fn elapsed_secs(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }
}
