//! Métriques Prometheus pour NIDAN.
//!
//! Expose un endpoint `/metrics` au format text/plain Prometheus.
//!
//! ## Métriques disponibles
//!
//! | Nom | Type | Description |
//! |-----|------|-------------|
//! | nidan_sessions_total | Counter | Sessions démarrées depuis le début |
//! | nidan_sessions_active | Gauge | Sessions actives en ce moment |
//! | nidan_frames_encoded_total | Counter | Frames encodées (serveur) |
//! | nidan_frames_decoded_total | Counter | Frames décodées (client) |
//! | nidan_auth_failures_total | Counter | Échecs d'authentification |
//! | nidan_clipboard_blocked_total | Counter | Transferts clipboard bloqués |
//! | nidan_policy_violations_total | Counter | Violations de politique |
//! | nidan_stream_latency_us | Histogram | Latence du stream vidéo (µs) |
//! | nidan_encode_duration_us | Histogram | Durée d'encodage (µs) |
//! | nidan_decode_duration_us | Histogram | Durée de décodage (µs) |
//! | nidan_pool_available | Gauge | VMs disponibles dans le pool |
//! | nidan_recording_bytes_total | Counter | Bytes écrits dans les enregistrements |

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{extract::State, routing::get, Router};
use prometheus::{
    Counter, Gauge, Histogram, HistogramOpts, IntCounter, IntGauge,
    Registry, TextEncoder,
};
use tracing::info;

use nidan_proto::v1::AuditEvent;

/// Registre de métriques NIDAN
pub struct NidanMetrics {
    registry: Registry,

    // Counters
    pub sessions_total:          IntCounter,
    pub auth_failures_total:     IntCounter,
    pub clipboard_blocked_total: IntCounter,
    pub policy_violations_total: IntCounter,
    pub frames_encoded_total:    IntCounter,
    pub frames_decoded_total:    IntCounter,
    pub recording_bytes_total:   IntCounter,

    // Gauges
    pub sessions_active:  IntGauge,
    pub pool_available:   IntGauge,

    // Histogrammes
    pub stream_latency_us:  Histogram,
    pub encode_duration_us: Histogram,
    pub decode_duration_us: Histogram,
}

impl NidanMetrics {
    /// Crée et enregistre toutes les métriques
    pub fn new() -> Result<Self> {
        let registry = Registry::new();

        // Counters
        let sessions_total = IntCounter::new(
            "nidan_sessions_total",
            "Nombre total de sessions démarrées"
        )?;
        let auth_failures_total = IntCounter::new(
            "nidan_auth_failures_total",
            "Nombre total d'échecs d'authentification"
        )?;
        let clipboard_blocked_total = IntCounter::new(
            "nidan_clipboard_blocked_total",
            "Nombre total de transferts clipboard bloqués"
        )?;
        let policy_violations_total = IntCounter::new(
            "nidan_policy_violations_total",
            "Nombre total de violations de politique"
        )?;
        let frames_encoded_total = IntCounter::new(
            "nidan_frames_encoded_total",
            "Nombre total de frames vidéo encodées"
        )?;
        let frames_decoded_total = IntCounter::new(
            "nidan_frames_decoded_total",
            "Nombre total de frames vidéo décodées"
        )?;
        let recording_bytes_total = IntCounter::new(
            "nidan_recording_bytes_total",
            "Bytes totaux écrits dans les enregistrements de session"
        )?;

        // Gauges
        let sessions_active = IntGauge::new(
            "nidan_sessions_active",
            "Nombre de sessions actives"
        )?;
        let pool_available = IntGauge::new(
            "nidan_pool_available",
            "Nombre de VMs disponibles dans le pool"
        )?;

        // Histogrammes avec buckets adaptés
        let latency_buckets = vec![
            100.0, 500.0, 1_000.0, 5_000.0, 10_000.0,
            33_000.0, 66_000.0, 100_000.0, 200_000.0,
        ];
        let stream_latency_us = Histogram::with_opts(
            HistogramOpts::new(
                "nidan_stream_latency_us",
                "Latence end-to-end du stream vidéo (µs)"
            ).buckets(latency_buckets.clone())
        )?;

        let encode_buckets = vec![
            500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0,
            20_000.0, 50_000.0, 100_000.0,
        ];
        let encode_duration_us = Histogram::with_opts(
            HistogramOpts::new(
                "nidan_encode_duration_us",
                "Durée d'encodage d'une frame vidéo (µs)"
            ).buckets(encode_buckets.clone())
        )?;
        let decode_duration_us = Histogram::with_opts(
            HistogramOpts::new(
                "nidan_decode_duration_us",
                "Durée de décodage d'une frame vidéo (µs)"
            ).buckets(encode_buckets)
        )?;

        // Enregistrement dans le registry
        registry.register(Box::new(sessions_total.clone()))?;
        registry.register(Box::new(auth_failures_total.clone()))?;
        registry.register(Box::new(clipboard_blocked_total.clone()))?;
        registry.register(Box::new(policy_violations_total.clone()))?;
        registry.register(Box::new(frames_encoded_total.clone()))?;
        registry.register(Box::new(frames_decoded_total.clone()))?;
        registry.register(Box::new(recording_bytes_total.clone()))?;
        registry.register(Box::new(sessions_active.clone()))?;
        registry.register(Box::new(pool_available.clone()))?;
        registry.register(Box::new(stream_latency_us.clone()))?;
        registry.register(Box::new(encode_duration_us.clone()))?;
        registry.register(Box::new(decode_duration_us.clone()))?;

        Ok(Self {
            registry,
            sessions_total,
            auth_failures_total,
            clipboard_blocked_total,
            policy_violations_total,
            frames_encoded_total,
            frames_decoded_total,
            recording_bytes_total,
            sessions_active,
            pool_available,
            stream_latency_us,
            encode_duration_us,
            decode_duration_us,
        })
    }

    // ── Méthodes de mise à jour ───────────────────────────────────────────────

    pub fn session_started(&self) {
        self.sessions_total.inc();
        self.sessions_active.inc();
    }

    pub fn session_ended(&self) {
        self.sessions_active.dec();
    }

    pub fn auth_failure(&self) {
        self.auth_failures_total.inc();
    }

    pub fn clipboard_blocked(&self) {
        self.clipboard_blocked_total.inc();
    }

    pub fn policy_violation(&self) {
        self.policy_violations_total.inc();
    }

    pub fn frame_encoded(&self, duration_us: f64) {
        self.frames_encoded_total.inc();
        self.encode_duration_us.observe(duration_us);
    }

    pub fn frame_decoded(&self, duration_us: f64) {
        self.frames_decoded_total.inc();
        self.decode_duration_us.observe(duration_us);
    }

    pub fn stream_latency(&self, latency_us: f64) {
        self.stream_latency_us.observe(latency_us);
    }

    pub fn recording_bytes(&self, bytes: i64) {
        self.recording_bytes_total.inc_by(bytes as u64);
    }

    pub fn set_pool_available(&self, count: i64) {
        self.pool_available.set(count);
    }

    /// Met à jour les métriques depuis un AuditEvent
    pub fn record_audit_event(&self, event: &AuditEvent) {
        use nidan_proto::v1::AuditEventType;
        match AuditEventType::try_from(event.event_type) {
            Ok(AuditEventType::SessionStart)      => self.session_started(),
            Ok(AuditEventType::SessionEnd)        => self.session_ended(),
            Ok(AuditEventType::AuthFailure)       => self.auth_failure(),
            Ok(AuditEventType::ClipboardBlocked)  => self.clipboard_blocked(),
            Ok(AuditEventType::PolicyViolation)   => self.policy_violation(),
            _ => {}
        }
    }

    /// Sérialise toutes les métriques au format Prometheus text
    pub fn render(&self) -> Result<String> {
        let encoder = TextEncoder::new();
        let families = self.registry.gather();
        encoder.encode_to_string(&families)
            .context("sérialisation métriques Prometheus")
    }
}

/// Démarre le serveur HTTP pour l'exposition des métriques
pub async fn run_metrics_server(
    bind_addr: String,
    metrics:   Arc<NidanMetrics>,
) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health",  get(|| async { "ok" }))
        .with_state(metrics);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await
        .with_context(|| format!("bind métriques sur {bind_addr}"))?;

    info!(addr = %bind_addr, "serveur métriques Prometheus démarré");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics_handler(
    State(metrics): State<Arc<NidanMetrics>>,
) -> Result<String, (axum::http::StatusCode, String)> {
    metrics.render().map_err(|e| (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        e.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_register_ok() {
        let m = NidanMetrics::new().unwrap();
        m.session_started();
        m.session_started();
        m.session_ended();
        m.auth_failure();
        m.frame_encoded(1500.0);
        m.frame_decoded(800.0);

        let output = m.render().unwrap();
        assert!(output.contains("nidan_sessions_total 2"));
        assert!(output.contains("nidan_sessions_active 1"));
        assert!(output.contains("nidan_auth_failures_total 1"));
        assert!(output.contains("nidan_frames_encoded_total 1"));
    }

    #[test]
    fn test_histogram_buckets_populated() {
        let m = NidanMetrics::new().unwrap();
        m.stream_latency(500.0);
        m.stream_latency(5000.0);
        m.stream_latency(33000.0);

        let output = m.render().unwrap();
        assert!(output.contains("nidan_stream_latency_us"));
        assert!(output.contains("nidan_stream_latency_us_count 3"));
    }
}
