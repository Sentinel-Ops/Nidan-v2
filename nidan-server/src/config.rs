//! Configuration du serveur NIDAN.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use nidan_common::config::{TlsConfig, VideoConfig, AuditConfig};

/// Configuration complète du serveur
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Configuration réseau
    pub network: NetworkConfig,
    /// Configuration de capture d'écran
    pub capture: CaptureConfig,
    /// Configuration vidéo (codec, bitrate, fps)
    pub video: VideoConfig,
    /// Configuration TLS/mTLS
    pub tls: TlsConfig,
    /// Configuration de l'audit
    pub audit: Option<AuditConfig>,
    /// Configuration de sécurité (seccomp, sandbox)
    pub security: SecurityConfig,
}

/// Configuration réseau du serveur
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Adresse d'écoute QUIC (ex: "0.0.0.0:7444")
    pub bind_addr: String,
    /// Timeout de session inactive (secondes)
    #[serde(default = "default_session_timeout")]
    pub session_timeout_secs: u64,
    /// Nombre maximum de connexions simultanées (mode multi-session futur)
    #[serde(default = "default_max_conns")]
    pub max_connections: usize,
}

fn default_session_timeout() -> u64 { 3600 }
fn default_max_conns() -> usize { 1 }

/// Configuration de la capture d'écran
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    /// Numéro du display X11 à capturer (ex: 100 pour :100)
    #[serde(default = "default_display")]
    pub display_number: u32,
    /// Utiliser XShm (shared memory, zéro copie) si disponible
    #[serde(default = "default_true")]
    pub use_xshm: bool,
    /// Utiliser XDamage (capture différentielle) si disponible
    #[serde(default = "default_true")]
    pub use_xdamage: bool,
    /// Nombre de frames maximum en attente dans le pipeline
    #[serde(default = "default_capture_queue")]
    pub capture_queue_depth: usize,
}

fn default_display() -> u32 { 100 }
fn default_true() -> bool { true }
fn default_capture_queue() -> usize { 4 }

/// Configuration de sécurité
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Activer le filtre seccomp-bpf après initialisation
    #[serde(default = "default_true")]
    pub seccomp_enabled: bool,
    /// Activer le chiffrement E2E ChaCha20-Poly1305 sur le flux vidéo
    #[serde(default = "default_true")]
    pub e2e_encryption: bool,
    /// Token de session attendu (fourni par le broker)
    /// En production, validé cryptographiquement — ici stocké pour dev
    pub session_token_file: Option<String>,
}

impl ServerConfig {
    /// Charge la configuration depuis un fichier TOML
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("lecture de {}", path.display()))?;
        let cfg: Self = toml::from_str(&content)
            .with_context(|| format!("parsing TOML de {}", path.display()))?;
        Ok(cfg)
    }

    /// Valide la cohérence de la configuration
    pub fn validate(&self) -> Result<()> {
        // Vérification de l'adresse de bind
        if self.network.bind_addr.is_empty() {
            bail!("network.bind_addr ne peut pas être vide");
        }
        // Vérification du codec
        match self.video.codec.as_str() {
            "h264" | "h265" | "hevc" | "av1" => {}
            other => bail!("codec inconnu: {other} (attendu: h264, h265, av1)"),
        }
        // Vérification des chemins TLS
        if !Path::new(&self.tls.ca_cert).exists() {
            bail!("tls.ca_cert introuvable: {}", self.tls.ca_cert);
        }
        if !Path::new(&self.tls.cert).exists() {
            bail!("tls.cert introuvable: {}", self.tls.cert);
        }
        if !Path::new(&self.tls.key).exists() {
            bail!("tls.key introuvable: {}", self.tls.key);
        }
        Ok(())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig {
                bind_addr: "0.0.0.0:7444".to_string(),
                session_timeout_secs: default_session_timeout(),
                max_connections: default_max_conns(),
            },
            capture: CaptureConfig {
                display_number:    default_display(),
                use_xshm:          true,
                use_xdamage:       true,
                capture_queue_depth: default_capture_queue(),
            },
            video: VideoConfig::default(),
            tls: TlsConfig {
                ca_cert: "/etc/nidan/certs/ca.crt".to_string(),
                cert:    "/etc/nidan/certs/server.crt".to_string(),
                key:     "/etc/nidan/certs/server.key".to_string(),
                allowed_client_domains: vec![],
            },
            audit: None,
            security: SecurityConfig {
                seccomp_enabled:  true,
                e2e_encryption:   true,
                session_token_file: None,
            },
        }
    }
}
