//! # nidan-proto
//!
//! Définitions du protocole NIDAN générées depuis `proto/nidan.proto`.
//!
//! Ce crate expose :
//! - Tous les types Protobuf générés par prost
//! - Des helpers de conversion et de validation
//! - Les constantes de version du protocole

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Version du protocole NIDAN. Doit correspondre entre client et serveur.
pub const PROTOCOL_VERSION: &str = "1.0.0";

/// Port UDP par défaut du broker NIDAN
pub const DEFAULT_BROKER_PORT: u16 = 7443;

/// Port UDP par défaut du serveur NIDAN (connexion directe hors broker)
pub const DEFAULT_SERVER_PORT: u16 = 7444;

/// Taille maximale d'une frame vidéo encodée (bytes) — 4 MB
pub const MAX_VIDEO_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Taille maximale d'un transfert clipboard (bytes) — 1 MB
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// Taille du nonce ChaCha20-Poly1305 (bytes)
pub const CHACHA20_NONCE_SIZE: usize = 12;

/// Taille de la clé symétrique dérivée (bytes)
pub const SESSION_KEY_SIZE: usize = 32;

/// Taille du nonce d'échange de clés (bytes)
pub const EXCHANGE_NONCE_SIZE: usize = 32;

// ─── Code généré par prost depuis nidan.proto ────────────────────────────────
// Le module est inclus depuis OUT_DIR à la compilation.
pub mod v1 {
    //! Messages Protobuf NIDAN version 1

    // Inclusion du code généré par tonic-build / prost-build
    tonic::include_proto!("nidan.v1");

    // Re-exports pratiques pour les consommateurs du crate
    pub use video_codec::*;
    pub use pixel_format::*;
    pub use input_event_type::*;
    pub use mouse_button::*;
    pub use clipboard_mime_type::*;
    pub use clipboard_direction::*;
    pub use auth_result::*;
    pub use auth_method::*;
    pub use audit_severity::*;
    pub use audit_event_type::*;
    pub use session_state::*;
}

// ─── Helpers de validation ────────────────────────────────────────────────────

/// Erreurs de validation des messages proto
#[derive(Debug, thiserror::Error)]
pub enum ProtoValidationError {
    /// Champ obligatoire manquant
    #[error("champ obligatoire manquant: {0}")]
    MissingField(&'static str),

    /// Valeur hors limites
    #[error("valeur hors limites pour {field}: {value} (max: {max})")]
    ValueTooLarge {
        /// Nom du champ
        field: &'static str,
        /// Valeur reçue
        value: usize,
        /// Valeur maximale autorisée
        max: usize,
    },

    /// Enum inconnue
    #[error("valeur d'enum inconnue pour {field}: {value}")]
    UnknownEnum {
        /// Nom du champ
        field: &'static str,
        /// Valeur reçue
        value: i32,
    },

    /// Format invalide
    #[error("format invalide pour {field}: {reason}")]
    InvalidFormat {
        /// Nom du champ
        field: &'static str,
        /// Raison
        reason: String,
    },
}

/// Trait de validation pour les messages proto
pub trait Validate {
    /// Valide le message. Retourne une erreur si le message est invalide.
    fn validate(&self) -> Result<(), ProtoValidationError>;
}

impl Validate for v1::ClientSessionRequest {
    fn validate(&self) -> Result<(), ProtoValidationError> {
        if self.client_version.is_empty() {
            return Err(ProtoValidationError::MissingField("client_version"));
        }
        if self.client_nonce.len() != EXCHANGE_NONCE_SIZE {
            return Err(ProtoValidationError::InvalidFormat {
                field: "client_nonce",
                reason: format!(
                    "attendu {} bytes, reçu {}",
                    EXCHANGE_NONCE_SIZE,
                    self.client_nonce.len()
                ),
            });
        }
        Ok(())
    }
}

impl Validate for v1::VideoFrame {
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
                reason: format!(
                    "attendu {} bytes, reçu {}",
                    CHACHA20_NONCE_SIZE,
                    self.nonce.len()
                ),
            });
        }
        Ok(())
    }
}

impl Validate for v1::ClipboardTransferRequest {
    fn validate(&self) -> Result<(), ProtoValidationError> {
        if self.session_id.is_empty() {
            return Err(ProtoValidationError::MissingField("session_id"));
        }
        if self.content.len() > MAX_CLIPBOARD_BYTES {
            return Err(ProtoValidationError::ValueTooLarge {
                field: "content",
                value: self.content.len(),
                max: MAX_CLIPBOARD_BYTES,
            });
        }
        Ok(())
    }
}

impl Validate for v1::InputEvent {
    fn validate(&self) -> Result<(), ProtoValidationError> {
        if self.event.is_none() {
            return Err(ProtoValidationError::MissingField("event"));
        }
        // Vérification des coordonnées normalisées souris
        if let Some(v1::input_event::Event::Mouse(ref m)) = self.event {
            if !(0.0..=1.0).contains(&m.x) || !(0.0..=1.0).contains(&m.y) {
                return Err(ProtoValidationError::InvalidFormat {
                    field: "mouse.x/y",
                    reason: format!(
                        "coordonnées normalisées attendues [0.0, 1.0], reçu ({}, {})",
                        m.x, m.y
                    ),
                });
            }
        }
        Ok(())
    }
}

// ─── Helpers de construction ──────────────────────────────────────────────────

/// Construit un message Ping avec timestamp courant
pub fn make_ping(seq: u64) -> v1::Ping {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    v1::Ping {
        seq,
        timestamp_us: ts,
    }
}

/// Construit un Pong en réponse à un Ping
pub fn make_pong(ping: &v1::Ping) -> v1::Pong {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    v1::Pong {
        seq: ping.seq,
        echo_timestamp_us: ping.timestamp_us,
        server_timestamp_us: ts,
    }
}

/// Calcule la latence aller-retour depuis un Pong (µs)
pub fn rtt_from_pong(pong: &v1::Pong) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    now.saturating_sub(pong.echo_timestamp_us)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_video_frame_validation_ok() {
        let frame = v1::VideoFrame {
            frame_seq: 1,
            encoded_data: vec![0u8; 1024],
            nonce: vec![0u8; CHACHA20_NONCE_SIZE],
            ..Default::default()
        };
        assert!(frame.validate().is_ok());
    }

    #[test]
    fn test_video_frame_empty_data_fails() {
        let frame = v1::VideoFrame {
            nonce: vec![0u8; CHACHA20_NONCE_SIZE],
            ..Default::default()
        };
        assert!(matches!(
            frame.validate(),
            Err(ProtoValidationError::MissingField("encoded_data"))
        ));
    }

    #[test]
    fn test_video_frame_too_large_fails() {
        let frame = v1::VideoFrame {
            encoded_data: vec![0u8; MAX_VIDEO_FRAME_BYTES + 1],
            nonce: vec![0u8; CHACHA20_NONCE_SIZE],
            ..Default::default()
        };
        assert!(matches!(
            frame.validate(),
            Err(ProtoValidationError::ValueTooLarge { field: "encoded_data", .. })
        ));
    }

    #[test]
    fn test_mouse_coords_normalized() {
        let ev = v1::InputEvent {
            event: Some(v1::input_event::Event::Mouse(v1::MouseEvent {
                x: 1.5,  // hors [0.0, 1.0]
                y: 0.5,
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(ev.validate().is_err());
    }

    #[test]
    fn test_ping_pong_rtt() {
        let ping = make_ping(42);
        assert_eq!(ping.seq, 42);
        let pong = make_pong(&ping);
        assert_eq!(pong.seq, 42);
        assert_eq!(pong.echo_timestamp_us, ping.timestamp_us);
        // RTT doit être très faible dans un test (< 100ms)
        let rtt = rtt_from_pong(&pong);
        assert!(rtt < 100_000, "RTT trop élevé: {} µs", rtt);
    }

    #[test]
    fn test_protocol_version_not_empty() {
        assert!(!PROTOCOL_VERSION.is_empty());
    }
}
