//! Configuration du client NIDAN.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nidan_common::config::{ClipboardPolicy, TlsConfig};

/// Configuration complète du client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub network:   NetworkConfig,
    pub video:     VideoConfig,
    pub display:   DisplayConfig,
    pub input:     InputConfig,
    pub clipboard: ClipboardPolicy,
    pub tls:       TlsConfig,
    pub security:  SecurityConfig,
}

/// Configuration réseau
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Adresse du broker (host:port)
    #[serde(default = "default_broker")]
    pub broker_addr: String,
    /// Timeout de connexion (secondes)
    #[serde(default = "default_timeout")]
    pub connect_timeout_secs: u64,
    /// Reconnexion automatique en cas de coupure
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    /// Délai entre tentatives de reconnexion (secondes)
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay_secs: u64,
    /// Connexion directe (dev) — bypasse le broker
    pub direct_server: Option<String>,
    /// Tag de VM préféré (optionnel) transmis au broker
    #[serde(default)]
    pub preferred_vm_tag: String,
}

fn default_broker() -> String { "localhost:7443".to_string() }
fn default_timeout() -> u64 { 10 }
fn default_true() -> bool { true }
fn default_reconnect_delay() -> u64 { 3 }

/// Configuration vidéo côté client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Codec préféré
    #[serde(default = "default_codec")]
    pub preferred_codec: String,
    /// FPS maximum souhaité
    #[serde(default = "default_fps")]
    pub max_fps: u32,
    /// Bitrate cible (kbps), 0 = adaptatif
    #[serde(default)]
    pub target_bitrate_kbps: u32,
    /// Utiliser le décodage hardware si disponible
    #[serde(default = "default_true")]
    pub hardware_decode: bool,
    /// Taille du buffer de décodage (frames)
    #[serde(default = "default_decode_buf")]
    pub decode_buffer_size: usize,
}

fn default_codec() -> String { "h264".to_string() }
fn default_fps() -> u32 { 30 }
fn default_decode_buf() -> usize { 4 }

/// Configuration d'affichage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// Mode plein écran
    #[serde(default)]
    pub fullscreen: bool,
    /// Mode seamless (fenêtres distantes dans le WM local)
    #[serde(default)]
    pub seamless: bool,
    /// Résolution forcée (None = auto depuis le serveur)
    pub force_width:  Option<u32>,
    pub force_height: Option<u32>,
    /// Scaling : "fit", "stretch", "1:1"
    #[serde(default = "default_scaling")]
    pub scaling: String,
    /// Titre de la fenêtre
    #[serde(default = "default_title")]
    pub window_title: String,
}

fn default_scaling() -> String { "fit".to_string() }
fn default_title() -> String { "NIDAN — Bureau distant sécurisé".to_string() }

/// Configuration des entrées
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    /// Capturer les raccourcis clavier système (ex: Alt+Tab)
    #[serde(default)]
    pub capture_system_shortcuts: bool,
    /// Sensibilité de la souris (multiplicateur)
    #[serde(default = "default_mouse_sensitivity")]
    pub mouse_sensitivity: f32,
    /// Activer le support tactile
    #[serde(default)]
    pub touch_enabled: bool,
    /// Touches de sortie du mode capturé (ex: "Ctrl+Alt+F")
    #[serde(default = "default_escape_key")]
    pub escape_combo: String,
}

fn default_mouse_sensitivity() -> f32 { 1.0 }
fn default_escape_key() -> String { "Ctrl+Alt+F".to_string() }

/// Configuration sécurité client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Vérifier le certificat serveur
    #[serde(default = "default_true")]
    pub verify_server_cert: bool,
    /// Activer le chiffrement E2E
    #[serde(default = "default_true")]
    pub e2e_encryption: bool,
    /// Méthode d'auth préférée
    #[serde(default = "default_auth")]
    pub auth_method: String,
}

fn default_auth() -> String { "mtls".to_string() }

impl ClientConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("lecture {}", path.as_ref().display()))?;
        toml::from_str(&content).context("parsing TOML config client")
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig {
                broker_addr:          default_broker(),
                preferred_vm_tag:     String::new(),
                connect_timeout_secs: default_timeout(),
                auto_reconnect:       true,
                reconnect_delay_secs: default_reconnect_delay(),
                direct_server:        None,
            },
            video: VideoConfig {
                preferred_codec:    default_codec(),
                max_fps:            default_fps(),
                target_bitrate_kbps: 0,
                hardware_decode:    true,
                decode_buffer_size: default_decode_buf(),
            },
            display: DisplayConfig {
                fullscreen:    false,
                seamless:      false,
                force_width:   None,
                force_height:  None,
                scaling:       default_scaling(),
                window_title:  default_title(),
            },
            input: InputConfig {
                capture_system_shortcuts: false,
                mouse_sensitivity:        1.0,
                touch_enabled:            false,
                escape_combo:             default_escape_key(),
            },
            clipboard: ClipboardPolicy::default(),
            tls: TlsConfig {
                ca_cert: "/etc/nidan/certs/ca.crt".to_string(),
                cert:    "/etc/nidan/certs/client.crt".to_string(),
                key:     "/etc/nidan/certs/client.key".to_string(),
                allowed_client_domains: vec![],
            },
            security: SecurityConfig {
                verify_server_cert: true,
                e2e_encryption:     true,
                auth_method:        default_auth(),
            },
        }
    }
}
