//! Vérification du jeton de session (JWT) délivré par le broker.
//!
//! Le broker signe un JWT (HS256) après authentification et attribution de VM.
//! Le serveur partage le même secret et vérifie ce jeton au handshake : si le
//! jeton est absent, invalide ou expiré, la session est refusée. Cela empêche
//! un client de contourner le broker en se connectant directement à une VM.

use anyhow::{bail, Context, Result};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

/// Claims du jeton de session — doit correspondre à celui émis par le broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaims {
    pub sub: String,
    pub iss: String,
    pub iat: i64,
    pub exp: i64,
    pub session_id: String,
    pub vm_id: String,
    pub auth_method: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub realm: Option<String>,
}

/// Vérifie un jeton de session JWT avec le secret partagé.
///
/// Contrôle la signature (HS256) et l'expiration. Retourne les claims si valide.
pub fn verify_session_token(token: &str, secret: &str) -> Result<SessionClaims> {
    if secret.is_empty() {
        bail!("secret JWT serveur non configuré");
    }
    let key = DecodingKey::from_secret(secret.as_bytes());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    // Le broker peut être un autre service : on ne fige pas l'audience ici,
    // mais l'expiration et la signature suffisent à garantir l'autorisation.
    validation.set_required_spec_claims(&["exp"]);

    let data = decode::<SessionClaims>(token, &key, &validation)
        .context("jeton de session invalide ou expiré")?;
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    fn make_token(secret: &str, exp_offset: i64) -> String {
        let now = chrono::Utc::now().timestamp();
        let claims = SessionClaims {
            sub: "user42".into(),
            iss: "nidan-broker".into(),
            iat: now,
            exp: now + exp_offset,
            session_id: "sess-123".into(),
            vm_id: "vm-01".into(),
            auth_method: "mtls".into(),
            groups: vec![],
            realm: None,
        };
        encode(&Header::new(Algorithm::HS256), &claims,
               &EncodingKey::from_secret(secret.as_bytes())).unwrap()
    }

    #[test]
    fn test_valid_token_accepted() {
        let secret = "shared_secret_minimum_32_chars_ok!!";
        let token = make_token(secret, 3600);
        let claims = verify_session_token(&token, secret).unwrap();
        assert_eq!(claims.vm_id, "vm-01");
        assert_eq!(claims.sub, "user42");
    }

    #[test]
    fn test_wrong_secret_rejected() {
        let token = make_token("shared_secret_minimum_32_chars_ok!!", 3600);
        assert!(verify_session_token(&token, "un_autre_secret_completement_faux!").is_err());
    }

    #[test]
    fn test_expired_token_rejected() {
        let secret = "shared_secret_minimum_32_chars_ok!!";
        // Au-delà de la tolérance d'horloge (leeway 60s) de jsonwebtoken
        let token = make_token(secret, -3600); // expiré depuis 1h
        assert!(verify_session_token(&token, secret).is_err());
    }

    #[test]
    fn test_empty_secret_rejected() {
        let token = make_token("shared_secret_minimum_32_chars_ok!!", 3600);
        assert!(verify_session_token(&token, "").is_err());
    }
}
