//! Génération et validation des session tokens JWT.
//!
//! NIDAN utilise HMAC-SHA256 (HS256) pour signer les tokens de session.
//! Ces tokens sont à courte durée de vie (5 min par défaut) et servent
//! à autoriser la connexion client → serveur après validation broker.

use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use super::AuthIdentity;

/// Claims du session token NIDAN
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaims {
    /// Subject = user_id
    pub sub: String,
    /// Issuer
    pub iss: String,
    /// Issued at (Unix timestamp)
    pub iat: i64,
    /// Expiration (Unix timestamp)
    pub exp: i64,
    /// Session ID (UUID v4)
    pub session_id: String,
    /// VM assignée
    pub vm_id: String,
    /// Méthode d'auth utilisée
    pub auth_method: String,
    /// Groupes
    pub groups: Vec<String>,
    /// Realm (Kerberos/OIDC)
    pub realm: Option<String>,
}

/// Moteur JWT
pub struct JwtEngine {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    issuer:       String,
    ttl_secs:     u64,
    validation:   Validation,
}

impl JwtEngine {
    pub fn new(secret: String, issuer: String, ttl_secs: u64) -> Self {
        let encoding_key = EncodingKey::from_secret(secret.as_bytes());
        let decoding_key = DecodingKey::from_secret(secret.as_bytes());

        let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.set_issuer(&[&issuer]);
        validation.set_required_spec_claims(&["sub", "exp", "iss", "session_id"]);
        validation.leeway = 0;

        Self { encoding_key, decoding_key, issuer, ttl_secs, validation }
    }

    /// Signe un nouveau session token
    pub fn sign(
        &self,
        identity:   &AuthIdentity,
        session_id: &str,
        vm_id:      &str,
    ) -> Result<String> {
        let now = Utc::now();
        let exp = now + Duration::seconds(self.ttl_secs as i64);

        let claims = SessionClaims {
            sub:         identity.user_id.clone(),
            iss:         self.issuer.clone(),
            iat:         now.timestamp(),
            exp:         exp.timestamp(),
            session_id:  session_id.to_string(),
            vm_id:       vm_id.to_string(),
            auth_method: identity.method.to_string(),
            groups:      identity.groups.clone(),
            realm:       identity.realm.clone(),
        };

        encode(&Header::default(), &claims, &self.encoding_key)
            .context("signature JWT")
    }

    /// Vérifie et décode un session token
    pub fn verify(&self, token: &str) -> Result<SessionClaims> {
        let data = decode::<SessionClaims>(token, &self.decoding_key, &self.validation)
            .context("validation JWT")?;
        Ok(data.claims)
    }

    /// Vérifie seulement la signature sans valider l'expiration
    /// (pour les cas de refresh — non utilisé en Phase 3)
    pub fn verify_ignore_expiry(&self, token: &str) -> Result<SessionClaims> {
        let mut validation = self.validation.clone();
        validation.validate_exp = false;
        let data = decode::<SessionClaims>(token, &self.decoding_key, &validation)
            .context("validation JWT (ignore expiry)")?;
        Ok(data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthIdentity, AuthMethodUsed};
    use std::net::IpAddr;

    fn make_identity() -> AuthIdentity {
        AuthIdentity {
            user_id:          "jo.dupont".to_string(),
            display_name:     Some("Jo Dupont".to_string()),
            groups:           vec!["ops".to_string()],
            method:           AuthMethodUsed::Mtls,
            realm:            None,
            authenticated_at: Utc::now(),
            client_ip:        "127.0.0.1".parse::<IpAddr>().unwrap(),
        }
    }

    #[test]
    fn test_sign_verify_roundtrip() {
        let engine = JwtEngine::new(
            "super_secret_key_minimum_32_chars!".to_string(),
            "nidan-broker".to_string(),
            300,
        );
        let identity = make_identity();
        let token = engine.sign(&identity, "sess-001", "vm-001").unwrap();
        assert!(!token.is_empty());

        let claims = engine.verify(&token).unwrap();
        assert_eq!(claims.sub, "jo.dupont");
        assert_eq!(claims.session_id, "sess-001");
        assert_eq!(claims.vm_id, "vm-001");
        assert_eq!(claims.auth_method, "mTLS");
    }

    #[test]
    fn test_expired_token_rejected() {
        // TTL = 0 → expiration immédiate
        let engine = JwtEngine::new(
            "super_secret_key_minimum_32_chars!".to_string(),
            "nidan-broker".to_string(),
            0,
        );
        let token = engine.sign(&make_identity(), "s", "v").unwrap();
        // Attendre pour garantir l'expiration (leeway=0)
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(engine.verify(&token).is_err(), "token expiré doit être rejeté");
    }

    #[test]
    fn test_tampered_token_rejected() {
        let engine = JwtEngine::new(
            "super_secret_key_minimum_32_chars!".to_string(),
            "nidan-broker".to_string(),
            300,
        );
        let mut token = engine.sign(&make_identity(), "s", "v").unwrap();
        // Altération du payload
        token.push_str("x");
        assert!(engine.verify(&token).is_err());
    }

    #[test]
    fn test_wrong_secret_rejected() {
        let engine1 = JwtEngine::new(
            "secret_key_number_one_32_chars!!!".to_string(),
            "nidan-broker".to_string(), 300,
        );
        let engine2 = JwtEngine::new(
            "secret_key_number_two_32_chars!!!".to_string(),
            "nidan-broker".to_string(), 300,
        );
        let token = engine1.sign(&make_identity(), "s", "v").unwrap();
        assert!(engine2.verify(&token).is_err(), "mauvaise clé doit être rejetée");
    }
}
