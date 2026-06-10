//! # nidan-proto
//!
//! Types du protocole NIDAN — définis en Rust pur.
//! (Pas de génération protobuf requise pour compiler)

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Version du protocole
pub const PROTOCOL_VERSION: &str = "1.0.0";
pub const DEFAULT_BROKER_PORT: u16 = 7443;
pub const DEFAULT_SERVER_PORT: u16 = 7444;
pub const MAX_VIDEO_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
pub const CHACHA20_NONCE_SIZE: usize = 12;
pub const SESSION_KEY_SIZE: usize = 32;
pub const EXCHANGE_NONCE_SIZE: usize = 32;

// ── Enums ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum VideoCodec {
    #[default]
    Unspecified = 0,
    H264 = 1,
    H265 = 2,
    Av1  = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum PixelFormat {
    #[default]
    Unspecified = 0,
    Yuv420p = 1,
    Yuv444p = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum AuthResult {
    #[default]
    Unspecified = 0,
    Success  = 1,
    Failure  = 2,
    MfaNeeded = 3,
    Expired  = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum AuthMethod {
    #[default]
    Unspecified = 0,
    Mtls     = 1,
    Kerberos = 2,
    Oidc     = 3,
    Saml     = 4,
}

impl TryFrom<i32> for AuthMethod {
    type Error = ();
    fn try_from(v: i32) -> Result<Self, ()> {
        match v {
            0 => Ok(Self::Unspecified),
            1 => Ok(Self::Mtls),
            2 => Ok(Self::Kerberos),
            3 => Ok(Self::Oidc),
            4 => Ok(Self::Saml),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum SessionState {
    #[default]
    Unspecified  = 0,
    Init         = 1,
    Auth         = 2,
    Active       = 3,
    Paused       = 4,
    Closing      = 5,
    Closed       = 6,
    Error        = 7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum AuditSeverity {
    #[default]
    Unspecified = 0,
    Debug    = 1,
    Info     = 2,
    Warning  = 3,
    Error    = 4,
    Critical = 5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum AuditEventType {
    #[default]
    Unspecified        = 0,
    SessionStart       = 1,
    SessionEnd         = 2,
    AuthSuccess        = 3,
    AuthFailure        = 4,
    ClipboardTransfer  = 5,
    ClipboardBlocked   = 6,
    InputAnomaly       = 7,
    ResolutionChange   = 8,
    VmAssigned         = 9,
    VmReleased         = 10,
    PolicyViolation    = 11,
    StreamInterrupted  = 12,
}

impl TryFrom<i32> for AuditEventType {
    type Error = ();
    fn try_from(v: i32) -> Result<Self, ()> {
        match v {
            0  => Ok(Self::Unspecified),
            1  => Ok(Self::SessionStart),
            2  => Ok(Self::SessionEnd),
            3  => Ok(Self::AuthSuccess),
            4  => Ok(Self::AuthFailure),
            5  => Ok(Self::ClipboardTransfer),
            6  => Ok(Self::ClipboardBlocked),
            7  => Ok(Self::InputAnomaly),
            8  => Ok(Self::ResolutionChange),
            9  => Ok(Self::VmAssigned),
            10 => Ok(Self::VmReleased),
            11 => Ok(Self::PolicyViolation),
            12 => Ok(Self::StreamInterrupted),
            _  => Err(()),
        }
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Frame vidéo encodée
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VideoFrame {
    pub frame_seq:          u64,
    pub monitor_index:      u32,
    pub codec:              i32,
    pub pixel_format:       i32,
    pub keyframe:           bool,
    pub encoded_data:       Vec<u8>,
    pub nonce:              Vec<u8>,
    pub width:              u32,
    pub height:             u32,
    pub pts_ms:             u32,
    pub encode_duration_us: u32,
    pub damage_hint:        Vec<u8>,
}

/// Demande de session client → broker
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientSessionRequest {
    pub client_version:    String,
    pub auth_method:       i32,
    pub auth_token:        Vec<u8>,
    pub preferred_vm_tag:  String,
    pub session_label:     String,
    pub client_nonce:      Vec<u8>,
}

/// Réponse du broker à une demande de session
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BrokerSessionResponse {
    pub auth_result:      i32,
    pub session_id:       String,
    pub vm_id:            String,
    pub server_address:   String,
    pub session_token:    Vec<u8>,
    pub server_nonce:     Vec<u8>,
    pub server_public_key: Vec<u8>,
    pub error_message:    String,
}

/// Handshake client → serveur
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientServerHandshake {
    pub session_id:          String,
    pub session_token:       Vec<u8>,
    pub preferred_codec:     i32,
    pub preferred_format:    i32,
    pub target_fps:          u32,
    pub target_bitrate_kbps: u32,
    pub audio_enabled:       bool,
    pub seamless_mode:       bool,
}

/// ACK du serveur au handshake
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerHandshakeAck {
    pub accepted:       bool,
    pub selected_codec: i32,
    pub selected_format: i32,
    pub state:          i32,
    pub stream_id:      u32,
    pub error_message:  String,
}

/// Événement d'entrée utilisateur
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InputEvent {
    pub seq:          u64,
    pub event_type:   i32,
    pub timestamp_ms: u32,
    pub event:        Option<InputEventPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEventPayload {
    Key(KeyEvent),
    Mouse(MouseEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeyEvent {
    pub keycode:  u32,
    pub scancode: u32,
    pub shift:    bool,
    pub ctrl:     bool,
    pub alt:      bool,
    pub meta:     bool,
    pub repeat:   bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MouseEvent {
    pub button:      i32,
    pub x:           f32,
    pub y:           f32,
    pub scroll_dx:   f32,
    pub scroll_dy:   f32,
    pub monitor_idx: u32,
}

// re-export pour compatibilité avec le code existant
pub mod input_event {
    use super::InputEventPayload;
    pub type Event = InputEventPayload;
}

/// Batch d'événements d'entrée
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InputBatch {
    pub events: Vec<InputEvent>,
}

/// Transfert clipboard
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClipboardTransferRequest {
    pub session_id:   String,
    pub direction:    i32,
    pub mime_type:    i32,
    pub content:      Vec<u8>,
    pub content_hash: u64,
    pub size_bytes:   u32,
}

/// Événement d'audit
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuditEvent {
    pub event_id:   String,
    pub event_type: i32,
    pub severity:   i32,
    pub session_id: String,
    pub user_id:    String,
    pub vm_id:      String,
    pub client_ip:  String,
    pub description: String,
}

/// Ping
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Ping {
    pub seq:          u64,
    pub timestamp_us: u64,
}

/// Pong
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Pong {
    pub seq:                 u64,
    pub echo_timestamp_us:   u64,
    pub server_timestamp_us: u64,
}

// ── Sérialisation length-prefixed ─────────────────────────────────────────────

/// Encode un message en JSON length-prefixed (4 bytes BE + payload)
pub fn encode_message<T: serde::Serialize>(msg: &T) -> anyhow::Result<Vec<u8>> {
    let payload = serde_json::to_vec(msg)?;
    let len = (payload.len() as u32).to_be_bytes();
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Décode un message JSON depuis un buffer (sans le préfixe longueur)
pub fn decode_message<T: serde::de::DeserializeOwned>(buf: &[u8]) -> anyhow::Result<T> {
    Ok(serde_json::from_slice(buf)?)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn make_ping(seq: u64) -> Ping {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    Ping { seq, timestamp_us: ts }
}

pub fn make_pong(ping: &Ping) -> Pong {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    Pong { seq: ping.seq, echo_timestamp_us: ping.timestamp_us, server_timestamp_us: ts }
}

pub fn rtt_from_pong(pong: &Pong) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    now.saturating_sub(pong.echo_timestamp_us)
}

// ── Validation ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ProtoValidationError {
    #[error("champ obligatoire manquant: {0}")]
    MissingField(&'static str),
    #[error("valeur hors limites pour {field}: {value} (max: {max})")]
    ValueTooLarge { field: &'static str, value: usize, max: usize },
    #[error("format invalide pour {field}: {reason}")]
    InvalidFormat { field: &'static str, reason: String },
}

pub trait Validate {
    fn validate(&self) -> Result<(), ProtoValidationError>;
}

impl Validate for VideoFrame {
    fn validate(&self) -> Result<(), ProtoValidationError> {
        if self.encoded_data.is_empty() {
            return Err(ProtoValidationError::MissingField("encoded_data"));
        }
        if self.encoded_data.len() > MAX_VIDEO_FRAME_BYTES {
            return Err(ProtoValidationError::ValueTooLarge {
                field: "encoded_data",
                value: self.encoded_data.len(),
                max: MAX_VIDEO_FRAME_BYTES,
            });
        }
        if self.nonce.len() != CHACHA20_NONCE_SIZE {
            return Err(ProtoValidationError::InvalidFormat {
                field: "nonce",
                reason: format!("attendu {} bytes, reçu {}", CHACHA20_NONCE_SIZE, self.nonce.len()),
            });
        }
        Ok(())
    }
}

impl Validate for InputEvent {
    fn validate(&self) -> Result<(), ProtoValidationError> {
        if let Some(InputEventPayload::Mouse(ref m)) = self.event {
            if !(0.0..=1.0).contains(&m.x) || !(0.0..=1.0).contains(&m.y) {
                return Err(ProtoValidationError::InvalidFormat {
                    field: "mouse.x/y",
                    reason: format!("attendu [0.0,1.0], reçu ({},{})", m.x, m.y),
                });
            }
        }
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let frame = VideoFrame {
            frame_seq: 42,
            encoded_data: vec![1, 2, 3],
            nonce: vec![0u8; CHACHA20_NONCE_SIZE],
            ..Default::default()
        };
        let encoded = encode_message(&frame).unwrap();
        assert!(encoded.len() > 4);
        let decoded: VideoFrame = decode_message(&encoded[4..]).unwrap();
        assert_eq!(decoded.frame_seq, 42);
    }

    #[test]
    fn test_video_frame_validate_ok() {
        let f = VideoFrame {
            encoded_data: vec![0u8; 100],
            nonce: vec![0u8; CHACHA20_NONCE_SIZE],
            ..Default::default()
        };
        assert!(f.validate().is_ok());
    }

    #[test]
    fn test_ping_pong() {
        let ping = make_ping(1);
        let pong = make_pong(&ping);
        assert_eq!(pong.seq, 1);
        assert!(rtt_from_pong(&pong) < 100_000);
    }

    #[test]
    fn test_auth_method_try_from() {
        assert_eq!(AuthMethod::try_from(1), Ok(AuthMethod::Mtls));
        assert_eq!(AuthMethod::try_from(2), Ok(AuthMethod::Kerberos));
        assert!(AuthMethod::try_from(99).is_err());
    }
}
