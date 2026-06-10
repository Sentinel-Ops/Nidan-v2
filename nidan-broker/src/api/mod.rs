//! API REST d'administration du broker NIDAN.
//!
//! Expose uniquement sur 127.0.0.1:7080 (non exposé publiquement).
//!
//! ## Endpoints
//! - GET  /health          → statut du broker
//! - GET  /api/pool        → état du pool de VMs
//! - GET  /api/sessions    → sessions actives
//! - POST /api/sessions/{id}/revoke → révocation de session
//! - GET  /metrics         → métriques Prometheus

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::routing::BrokerState;

pub async fn run_admin_api(state: Arc<BrokerState>) -> anyhow::Result<()> {
    let bind = state.config.network.admin_bind.clone();
    let metrics_enabled = state.config.admin.metrics_enabled;

    let mut router = Router::new()
        .route("/health",                get(health_handler))
        .route("/api/pool",              get(pool_status_handler))
        .route("/api/sessions",          get(sessions_handler))
        .route("/api/sessions/:id/revoke", post(revoke_session_handler));

    if metrics_enabled {
        router = router.route("/metrics", get(metrics_handler));
    }

    let router = router
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    info!(addr = %bind, "API admin démarrée");

    axum::serve(listener, router).await?;
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health_handler(State(state): State<Arc<BrokerState>>) -> Json<Value> {
    let pool   = state.pool.status();
    let status = if pool.available > 0 { "healthy" } else { "degraded" };

    Json(json!({
        "status":   status,
        "version":  env!("CARGO_PKG_VERSION"),
        "sessions": state.sessions.active_count(),
        "pool": {
            "total":     pool.total,
            "available": pool.available,
            "assigned":  pool.assigned,
            "unhealthy": pool.unhealthy,
        }
    }))
}

async fn pool_status_handler(State(state): State<Arc<BrokerState>>) -> Json<Value> {
    let vms: Vec<Value> = state.pool.all_vms().into_iter().map(|vm| {
        json!({
            "id":       vm.id,
            "addr":     vm.addr(),
            "tags":     vm.tags,
            "state":    vm.state.label(),
            "sessions": vm.sessions_served,
            "last_health": vm.last_health.map(|t| t.to_rfc3339()),
        })
    }).collect();

    let status = state.pool.status();
    Json(json!({
        "total":     status.total,
        "available": status.available,
        "assigned":  status.assigned,
        "unhealthy": status.unhealthy,
        "vms":       vms,
    }))
}

async fn sessions_handler(State(state): State<Arc<BrokerState>>) -> Json<Value> {
    let sessions: Vec<Value> = state.sessions.all().into_iter().map(|s| {
        json!({
            "id":          s.id,
            "user_id":     s.user_id,
            "client_ip":   s.client_ip,
            "vm_id":       s.vm_id,
            "state":       format!("{:?}", s.state),
            "auth_method": s.auth_method,
            "started_at":  s.started_at.to_rfc3339(),
        })
    }).collect();

    Json(json!({ "count": sessions.len(), "sessions": sessions }))
}

#[derive(Deserialize)]
struct RevokeBody {
    reason: Option<String>,
}

async fn revoke_session_handler(
    State(state): State<Arc<BrokerState>>,
    Path(session_id): Path<String>,
    Json(body): Json<Option<RevokeBody>>,
) -> (StatusCode, Json<Value>) {
    let reason = body
        .and_then(|b| b.reason)
        .unwrap_or_else(|| "révoquée par admin".to_string());

    if state.sessions.revoke(&session_id, &reason) {
        (StatusCode::OK, Json(json!({ "revoked": true, "session_id": session_id })))
    } else {
        (StatusCode::NOT_FOUND, Json(json!({ "error": "session introuvable" })))
    }
}

async fn metrics_handler(State(state): State<Arc<BrokerState>>) -> String {
    let pool     = state.pool.status();
    let sessions = state.sessions.active_count();

    // Format Prometheus text
    format!(
        "# HELP nidan_pool_total Total de VMs dans le pool\n\
         # TYPE nidan_pool_total gauge\n\
         nidan_pool_total {}\n\
         # HELP nidan_pool_available VMs disponibles\n\
         # TYPE nidan_pool_available gauge\n\
         nidan_pool_available {}\n\
         # HELP nidan_sessions_active Sessions actives\n\
         # TYPE nidan_sessions_active gauge\n\
         nidan_sessions_active {}\n\
         # HELP nidan_pool_unhealthy VMs hors service\n\
         # TYPE nidan_pool_unhealthy gauge\n\
         nidan_pool_unhealthy {}\n",
        pool.total,
        pool.available,
        sessions,
        pool.unhealthy,
    )
}
