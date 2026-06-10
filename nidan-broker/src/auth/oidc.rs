//! Validation des Bearer tokens OIDC.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::config::OidcConfig;

/// Valide un Bearer JWT OIDC et retourne les claims
///
/// En Phase 3 : valide la signature via JWKS endpoint du provider
/// En stub : valide seulement le format et l'audience
pub async fn validate_bearer_token(
    token: &str,
    config: &OidcConfig,
) -> Result<std::collections::HashMap<String, Value>> {
    // TODO Phase 3.1 : récupérer JWKS depuis config.issuer_url + "/.well-known/jwks.json"
    // et valider la signature RS256/ES256

    // Stub : décode le payload sans valider la signature
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        bail!("format JWT invalide");
    }

    // Décodage Base64URL du payload (partie centrale)
    let payload_b64 = parts[1];
    let payload_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload_b64,
    )?;
    let claims: std::collections::HashMap<String, Value> =
        serde_json::from_slice(&payload_bytes)?;

    // Vérification de l'audience
    if let Some(aud) = claims.get("aud") {
        let aud_matches = match aud {
            Value::String(s) => s == &config.audience,
            Value::Array(arr) => arr.iter().any(|v| {
                v.as_str().map(|s| s == config.audience).unwrap_or(false)
            }),
            _ => false,
        };
        if !aud_matches {
            bail!("audience JWT invalide");
        }
    }

    // Vérification de l'expiration
    if let Some(exp) = claims.get("exp").and_then(|v| v.as_i64()) {
        let now = chrono::Utc::now().timestamp();
        if now > exp {
            bail!("token OIDC expiré");
        }
    }

    Ok(claims)
}
