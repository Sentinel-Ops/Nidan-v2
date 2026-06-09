//! Configuration partagée pour tous les composants NIDAN.
//!
//! Chaque composant embed un sous-ensemble de cette config dans son propre
//! fichier `nidan-server.toml`, `nidan-client.toml`, `nidan-broker.toml`.

use serde::{Deserialize, Serialize};

/// Configuration TLS/mTLS commune
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Chemin vers le certificat CA (pour valider les pairs)
    pub ca_cert: String,
    /// Chemin vers le certificat local
    pub cert:    String,
    /// Chemin vers la clé privée locale
    pub key:     String,
    /// Domaines clients autorisés (optionnel, ex: ["interne.example.fr"])
    #[serde(default)]
    pub allowed_client_domains: Vec<String>,
}

/// Configuration du transport QUIC
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuicConfig {
    /// Timeout de connexion (secondes)
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Timeout keepalive (secondes)
    #[serde(default = "default_keepalive")]
    pub keepalive_interval_secs: u64,
    /// Taille maximale de datagramme UDP (bytes)
    #[serde(default = "default_max_datagram")]
    pub max_datagram_size: usize,
    /// Taille du buffer de réception (bytes)
    #[serde(default = "default_recv_buf")]
    pub recv_buffer_size: usize,
}

fn default_connect_timeout() -> u64 { 10 }
fn default_keepalive() -> u64 { 5 }
fn default_max_datagram() -> usize { 1350 }
fn default_recv_buf() -> usize { 8 * 1024 * 1024 } // 8 MB

impl Default for QuicConfig {
    fn default() -> Self {
        Self {
            connect_timeout_secs:    default_connect_timeout(),
            keepalive_interval_secs: default_keepalive(),
            max_datagram_size:       default_max_datagram(),
            recv_buffer_size:        default_recv_buf(),
        }
    }
}

/// Configuration vidéo partagée
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Codec préféré
    #[serde(default = "default_codec")]
    pub codec: String,
    /// Bitrate cible (kbps), 0 = adaptatif
    #[serde(default)]
    pub target_bitrate_kbps: u32,
    /// FPS maximum, 0 = illimité
    #[serde(default = "default_fps")]
    pub max_fps: u32,
    /// Format de pixel
    #[serde(default = "default_pixel_fmt")]
    pub pixel_format: String,
    /// Utiliser l'accélération matérielle si disponible
    #[serde(default = "default_true")]
    pub hardware_accel: bool,
}

fn default_codec() -> String { "h264".to_string() }
fn default_fps() -> u32 { 30 }
fn default_pixel_fmt() -> String { "yuv420p".to_string() }
fn default_true() -> bool { true }

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            codec:               default_codec(),
            target_bitrate_kbps: 0,
            max_fps:             default_fps(),
            pixel_format:        default_pixel_fmt(),
            hardware_accel:      true,
        }
    }
}

/// Configuration de la politique clipboard
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClipboardPolicy {
    /// Autoriser le transfert client → serveur
    #[serde(default = "default_true")]
    pub allow_client_to_server: bool,
    /// Autoriser le transfert serveur → client
    #[serde(default = "default_true")]
    pub allow_server_to_client: bool,
    /// Taille maximale autorisée (bytes), 0 = pas de limite (sauf MAX_CLIPBOARD_BYTES)
    #[serde(default)]
    pub max_size_bytes: u32,
    /// Types MIME autorisés (vide = tous autorisés)
    #[serde(default)]
    pub allowed_mime_types: Vec<String>,
    /// Patterns regex bloqués dans le contenu (ex: clés PEM, numéros CB)
    #[serde(default)]
    pub blocked_patterns: Vec<String>,
    /// Journaliser chaque transfert clipboard
    #[serde(default = "default_true")]
    pub audit_transfers: bool,
}

/// Configuration de l'audit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Répertoire de stockage des sessions enregistrées
    pub session_recording_dir: String,
    /// Activer le watermarking forensique
    #[serde(default = "default_true")]
    pub watermarking_enabled: bool,
    /// Endpoint Prometheus (ex: "0.0.0.0:9090")
    pub prometheus_endpoint: Option<String>,
    /// Serveur syslog distant (ex: "syslog.interne.fr:514")
    pub syslog_remote: Option<String>,
}
