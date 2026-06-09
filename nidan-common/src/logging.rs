//! Initialisation du système de logging structuré (tracing).

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialise le logging structuré.
///
/// Respecte la variable d'environnement `NIDAN_LOG` (ex: `NIDAN_LOG=debug`).
/// Format JSON en production, format lisible en développement.
///
/// # Exemple
/// ```
/// nidan_common::logging::init("nidan-server");
/// ```
pub fn init(component: &str) {
    let filter = EnvFilter::try_from_env("NIDAN_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let json_logs = std::env::var("NIDAN_LOG_JSON")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);

    if json_logs {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().json().with_current_span(true))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_target(true).with_thread_ids(false))
            .init();
    }

    tracing::info!(component = component, version = env!("CARGO_PKG_VERSION"), "NIDAN démarré");
}
