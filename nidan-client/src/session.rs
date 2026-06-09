//! État d'une session côté client.

use std::time::Instant;

use nidan_common::session::SessionId;

/// État de la session client
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClientSessionState {
    Connecting,
    Authenticating,
    Handshaking,
    Active,
    Reconnecting,
    Closing,
    Closed,
}

/// Métriques de session côté client
#[derive(Debug, Default)]
pub struct ClientSessionMetrics {
    pub frames_received:   u64,
    pub frames_decoded:    u64,
    pub frames_dropped:    u64,
    pub bytes_received:    u64,
    pub last_rtt_us:       u64,
    pub reconnect_count:   u32,
    pub avg_decode_us:     f64,
}

/// Session client complète
pub struct ClientSession {
    pub id:         SessionId,
    pub state:      ClientSessionState,
    pub started_at: Instant,
    pub metrics:    ClientSessionMetrics,
    /// Résolution négociée
    pub width:      u32,
    pub height:     u32,
}

impl ClientSession {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            id:         SessionId::new(),
            state:      ClientSessionState::Connecting,
            started_at: Instant::now(),
            metrics:    ClientSessionMetrics::default(),
            width,
            height,
        }
    }

    pub fn elapsed_secs(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    pub fn fps(&self) -> f32 {
        let elapsed = self.elapsed_secs();
        if elapsed > 0.0 {
            self.metrics.frames_decoded as f32 / elapsed as f32
        } else {
            0.0
        }
    }

    pub fn record_frame(&mut self, size: usize, decode_us: u32) {
        self.metrics.frames_received += 1;
        self.metrics.frames_decoded  += 1;
        self.metrics.bytes_received  += size as u64;
        // Moyenne mobile exponentielle α = 0.1
        self.metrics.avg_decode_us =
            0.9 * self.metrics.avg_decode_us + 0.1 * decode_us as f64;
    }
}
