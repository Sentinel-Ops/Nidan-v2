//! Configuration du daemon d'audit NIDAN.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditDaemonConfig {
    pub storage:   StorageConfig,
    pub watermark: WatermarkConfig,
    pub metrics:   MetricsConfig,
    pub recording: RecordingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Répertoire des enregistrements de sessions (MKV)
    #[serde(default = "default_session_dir")]
    pub session_dir: String,
    /// Répertoire des logs d'événements (JSONL)
    #[serde(default = "default_event_dir")]
    pub event_log_dir: String,
    /// Persister les événements sur disque
    #[serde(default = "default_true")]
    pub persist_events: bool,
    /// Clé HMAC pour sceller les fichiers (32 bytes hex)
    pub seal_key: Option<String>,
    /// Durée de rétention des enregistrements (jours, 0 = infini)
    #[serde(default)]
    pub retention_days: u32,
    /// Taille maximale du répertoire de sessions (MB, 0 = illimité)
    #[serde(default)]
    pub max_storage_mb: u64,
}

fn default_session_dir() -> String { "/var/lib/nidan/sessions".to_string() }
fn default_event_dir()   -> String { "/var/lib/nidan/events".to_string() }
fn default_true()        -> bool   { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatermarkConfig {
    /// Activer le watermarking stéganographique
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Force du watermark : nombre de bits LSB modifiés (1–4)
    #[serde(default = "default_wm_strength")]
    pub strength: u8,
    /// Intervalle entre watermarks (nombre de frames)
    #[serde(default = "default_wm_interval")]
    pub interval_frames: u32,
}

fn default_wm_strength()  -> u8  { 2 }
fn default_wm_interval()  -> u32 { 30 } // 1 seconde à 30fps

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// Adresse d'écoute Prometheus
    #[serde(default = "default_metrics_bind")]
    pub bind_addr: String,
    /// Intervalle de mise à jour (secondes)
    #[serde(default = "default_metrics_interval")]
    pub update_interval_secs: u64,
}

fn default_metrics_bind()     -> String { "0.0.0.0:9090".to_string() }
fn default_metrics_interval() -> u64   { 15 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingConfig {
    /// Activer l'enregistrement des sessions
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Format : "mkv" (défaut) ou "raw"
    #[serde(default = "default_format")]
    pub format: String,
    /// Qualité d'enregistrement (0–100, 0 = lossless)
    #[serde(default = "default_quality")]
    pub quality: u8,
    /// Enregistrer l'audio si disponible
    #[serde(default)]
    pub include_audio: bool,
}

fn default_format()  -> String { "mkv".to_string() }
fn default_quality() -> u8    { 80 }

impl AuditDaemonConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("lecture {}", path.as_ref().display()))?;
        toml::from_str(&content).context("parsing TOML config audit")
    }
}

impl Default for AuditDaemonConfig {
    fn default() -> Self {
        Self {
            storage: StorageConfig {
                session_dir:    default_session_dir(),
                event_log_dir:  default_event_dir(),
                persist_events: true,
                seal_key:       None,
                retention_days: 90,
                max_storage_mb: 0,
            },
            watermark: WatermarkConfig {
                enabled:         true,
                strength:        default_wm_strength(),
                interval_frames: default_wm_interval(),
            },
            metrics: MetricsConfig {
                bind_addr:            default_metrics_bind(),
                update_interval_secs: default_metrics_interval(),
            },
            recording: RecordingConfig {
                enabled:       true,
                format:        default_format(),
                quality:       default_quality(),
                include_audio: false,
            },
        }
    }
}
