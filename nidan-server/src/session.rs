//! État d'une session côté serveur.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use nidan_common::session::SessionId;
use nidan_proto::SessionState;

/// État d'une session côté serveur
pub struct ServerSession {
    pub id:         SessionId,
    pub state:      Arc<Mutex<SessionState>>,
    pub started_at: Instant,
    pub metrics:    Arc<Mutex<SessionMetrics>>,
}

/// Métriques temps réel de la session
#[derive(Debug, Default)]
pub struct SessionMetrics {
    pub frames_encoded: u64,
    pub frames_sent:    u64,
    pub frames_dropped: u64,
    pub bytes_sent:     u64,
    pub last_rtt_us:    u64,
    pub avg_encode_us:  f64,
}

impl SessionMetrics {
    pub fn record_frame(&mut self, size: usize, encode_us: u32) {
        self.frames_encoded += 1;
        self.bytes_sent     += size as u64;
        self.avg_encode_us   = 0.9 * self.avg_encode_us + 0.1 * encode_us as f64;
    }

    pub fn fps_actual(&self, elapsed_secs: f64) -> f32 {
        if elapsed_secs > 0.0 { (self.frames_sent as f64 / elapsed_secs) as f32 } else { 0.0 }
    }

    pub fn bitrate_kbps(&self, elapsed_secs: f64) -> u32 {
        if elapsed_secs > 0.0 {
            ((self.bytes_sent as f64 * 8.0) / (elapsed_secs * 1000.0)) as u32
        } else { 0 }
    }
}

impl ServerSession {
    pub fn new(id: SessionId) -> Self {
        tracing::info!(session_id = %id, "nouvelle session serveur créée");
        Self {
            id,
            state:      Arc::new(Mutex::new(SessionState::Active)),
            started_at: Instant::now(),
            metrics:    Arc::new(Mutex::new(SessionMetrics::default())),
        }
    }

    pub fn elapsed_secs(&self) -> f64 { self.started_at.elapsed().as_secs_f64() }
}
