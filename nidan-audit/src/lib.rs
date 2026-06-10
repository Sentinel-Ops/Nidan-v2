//! # nidan-audit
//!
//! Librairie d'audit NIDAN. Peut être embarquée directement dans
//! nidan-server (mode intégré) ou tournée en daemon séparé.
//!
//! ## Composants
//! - `recording` : enregistrement des sessions en MKV avec index temporel
//! - `watermark`  : injection stéganographique d'identifiants dans le flux vidéo
//! - `metrics`    : exposition Prometheus des métriques de session
//! - `storage`    : gestion des fichiers d'audit (WORM, rotation, scellage HMAC)

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod config;
pub mod metrics;
pub mod recording;
pub mod storage;
pub mod watermark;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::info;

use nidan_proto::v1::AuditEvent;

use config::AuditDaemonConfig;

/// Canal de réception des événements d'audit
pub type AuditEventSender = mpsc::Sender<AuditEvent>;

/// Moteur d'audit principal
pub struct AuditEngine {
    config:   AuditDaemonConfig,
    registry: Arc<metrics::NidanMetrics>,
}

impl AuditEngine {
    /// Crée et initialise le moteur d'audit
    pub async fn new(config: AuditDaemonConfig) -> Result<Self> {
        // Création des répertoires de stockage
        std::fs::create_dir_all(&config.storage.session_dir)
            .with_context(|| format!("création répertoire sessions: {}", config.storage.session_dir))?;
        std::fs::create_dir_all(&config.storage.event_log_dir)
            .with_context(|| format!("création répertoire events: {}", config.storage.event_log_dir))?;

        let registry = Arc::new(metrics::NidanMetrics::new()?);

        info!(
            session_dir  = %config.storage.session_dir,
            watermark    = config.watermark.enabled,
            prometheus   = %config.metrics.bind_addr,
            "moteur d'audit initialisé"
        );

        Ok(Self { config, registry })
    }

    /// Démarre tous les services d'audit
    pub async fn run(self) -> Result<()> {
        let (event_tx, event_rx) = mpsc::channel::<AuditEvent>(1024);
        let registry = self.registry.clone();
        let config   = self.config.clone();

        // Service de métriques Prometheus
        let metrics_bind = config.metrics.bind_addr.clone();
        let metrics_reg  = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = metrics::run_metrics_server(metrics_bind, metrics_reg).await {
                tracing::error!(error = %e, "serveur métriques arrêté");
            }
        });

        // Boucle de traitement des événements d'audit
        self.event_loop(event_rx, registry).await
    }

    /// Boucle principale de traitement des AuditEvent
    async fn event_loop(
        &self,
        mut rx: mpsc::Receiver<AuditEvent>,
        registry: Arc<metrics::NidanMetrics>,
    ) -> Result<()> {
        use nidan_proto::v1::AuditEventType;

        info!("boucle d'audit démarrée");

        while let Some(event) = rx.recv().await {
            // Mise à jour des métriques
            registry.record_audit_event(&event);

            // Journalisation structurée
            tracing::info!(
                event_id   = %event.event_id,
                event_type = ?event.event_type,
                session_id = %event.session_id,
                user_id    = %event.user_id,
                severity   = ?event.severity,
                "audit event"
            );

            // Persistance selon le type d'événement
            let event_type = AuditEventType::try_from(event.event_type)
                .unwrap_or(AuditEventType::Unspecified);

            match event_type {
                AuditEventType::SessionStart => {
                    registry.session_started();
                }
                AuditEventType::SessionEnd => {
                    registry.session_ended();
                }
                AuditEventType::AuthFailure => {
                    registry.auth_failure();
                }
                AuditEventType::ClipboardBlocked => {
                    registry.clipboard_blocked();
                }
                AuditEventType::PolicyViolation => {
                    tracing::warn!(
                        session_id = %event.session_id,
                        user_id    = %event.user_id,
                        "violation de politique de sécurité"
                    );
                    registry.policy_violation();
                }
                _ => {}
            }

            // Persistance sur disque (event log JSONL)
            if self.config.storage.persist_events {
                if let Err(e) = self.persist_event(&event).await {
                    tracing::warn!(error = %e, "erreur persistance événement audit");
                }
            }
        }

        info!("boucle d'audit terminée");
        Ok(())
    }

    /// Persiste un événement dans le log JSONL
    async fn persist_event(&self, event: &AuditEvent) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let log_path = PathBuf::from(&self.config.storage.event_log_dir)
            .join(format!("audit-{}.jsonl",
                chrono::Utc::now().format("%Y-%m-%d")));

        let json = serde_json::to_string(event)
            .context("sérialisation événement audit")?;

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
            .with_context(|| format!("ouverture {}", log_path.display()))?;

        file.write_all(json.as_bytes()).await?;
        file.write_all(b"\n").await?;

        Ok(())
    }
}

/// Crée un `AuditEventSender` pour qu'un composant puisse émettre des événements
pub fn make_audit_sender(engine: &AuditEngine) -> Option<AuditEventSender> {
    // En mode intégré : retourne un sender connecté au moteur
    // En mode daemon : retourne None (le composant contacte le daemon via gRPC)
    None // TODO Phase 4.1 : channel partagé via Arc<Mutex<>>
}
