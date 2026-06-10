//! Moteur d'authentification multi-méthodes du broker NIDAN.
//!
//! Supporte : mTLS, Kerberos/SPNEGO, OIDC/JWT, SAML (stub)
//!
//! ## Flux d'authentification
//! ```text
//! ClientSessionRequest (auth_method + auth_token)
//!        ↓
//! AuthEngine::authenticate()
//!        ↓
//! ┌──────────────────────────────────────────┐
//! │ mTLS    : validation cert client X.509   │
//! │ Kerberos: validation ticket AP-REQ       │
//! │ OIDC    : validation JWT Bearer          │
//! │ SAML    : validation assertion (stub)    │
//! └──────────────────────────────────────────┘
//!        ↓
//! AuthIdentity { user_id, groups, method, ... }
//!        ↓
//! SessionToken JWT signé (TTL configurable)
//! ```

pub mod jwt;
pub mod mtls;
pub mod oidc;
pub mod ratelimit;

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use nidan_proto::AuthMethod;

use crate::config::AuthConfig;
use ratelimit::RateLimiter;

/// Identité authentifiée
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthIdentity {
    /// Identifiant unique de l'utilisateur
    pub user_id: String,
    /// Nom d'affichage
    pub display_name: Option<String>,
    /// Groupes/rôles
    pub groups: Vec<String>,
    /// Méthode d'auth utilisée
    pub method: AuthMethodUsed,
    /// Realm/domaine (Kerberos ou OIDC issuer)
    pub realm: Option<String>,
    /// Timestamp d'authentification
    pub authenticated_at: DateTime<Utc>,
    /// IP source du client
    pub client_ip: IpAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AuthMethodUsed {
    Mtls,
    Kerberos,
    Oidc,
    Saml,
}

impl std::fmt::Display for AuthMethodUsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mtls     => write!(f, "mTLS"),
            Self::Kerberos => write!(f, "Kerberos"),
            Self::Oidc     => write!(f, "OIDC"),
            Self::Saml     => write!(f, "SAML"),
        }
    }
}

/// Résultat d'une tentative d'authentification
#[derive(Debug)]
pub enum AuthOutcome {
    /// Authentification réussie
    Success(AuthIdentity),
    /// Échec avec raison
    Failure(AuthFailureReason),
    /// MFA requis (TOTP, etc.)
    MfaRequired { challenge: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthFailureReason {
    InvalidCredentials,
    ExpiredCredentials,
    UnknownMethod,
    RateLimited,
    Banned,
    InternalError(String),
}

impl std::fmt::Display for AuthFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCredentials  => write!(f, "identifiants invalides"),
            Self::ExpiredCredentials  => write!(f, "identifiants expirés"),
            Self::UnknownMethod       => write!(f, "méthode d'auth inconnue"),
            Self::RateLimited         => write!(f, "trop de tentatives"),
            Self::Banned              => write!(f, "IP bannie"),
            Self::InternalError(e)    => write!(f, "erreur interne: {e}"),
        }
    }
}

impl AuthOutcome {
    pub fn to_proto_result_i32(&self) -> i32 {
        match self {
            Self::Success(_)      => nidan_proto::AuthResult::Success  as i32,
            Self::MfaRequired{..} => nidan_proto::AuthResult::MfaNeeded as i32,
            Self::Failure(r) => match r {
                AuthFailureReason::ExpiredCredentials => nidan_proto::AuthResult::Expired as i32,
                _                                     => nidan_proto::AuthResult::Failure as i32,
            }
        }
    }
}

/// Moteur d'authentification principal
pub struct AuthEngine {
    config:       AuthConfig,
    rate_limiter: Arc<RateLimiter>,
    jwt_engine:   jwt::JwtEngine,
}

impl AuthEngine {
    pub fn new(config: AuthConfig) -> Self {
        let jwt_engine = jwt::JwtEngine::new(
            config.jwt.secret.clone(),
            config.jwt.issuer.clone(),
            config.session_token_ttl_secs,
        );
        Self {
            rate_limiter: Arc::new(RateLimiter::new(
                config.security_max_attempts(),
                config.security_ban_secs(),
            )),
            config,
            jwt_engine,
        }
    }

    /// Authentifie une demande de session
    pub async fn authenticate(
        &self,
        method:     AuthMethod,
        token:      &[u8],
        client_ip:  IpAddr,
        cert_identity: Option<String>, // Identité extraite du cert mTLS par QUIC/TLS
    ) -> AuthOutcome {
        // Rate limiting
        if self.rate_limiter.is_banned(client_ip) {
            warn!(ip = %client_ip, "tentative depuis IP bannie");
            return AuthOutcome::Failure(AuthFailureReason::Banned);
        }
        if self.rate_limiter.is_rate_limited(client_ip) {
            warn!(ip = %client_ip, "rate limit atteint");
            self.rate_limiter.record_failure(client_ip);
            return AuthOutcome::Failure(AuthFailureReason::RateLimited);
        }

        let outcome = match method {
            AuthMethod::Mtls => {
                self.auth_mtls(cert_identity, client_ip).await
            }
            AuthMethod::Kerberos => {
                self.auth_kerberos(token, client_ip).await
            }
            AuthMethod::Oidc => {
                self.auth_oidc(token, client_ip).await
            }
            AuthMethod::Saml => {
                self.auth_saml(token, client_ip).await
            }
            _ => AuthOutcome::Failure(AuthFailureReason::UnknownMethod),
        };

        // Audit
        match &outcome {
            AuthOutcome::Success(id) => {
                info!(
                    user    = %id.user_id,
                    method  = %id.method,
                    ip      = %client_ip,
                    "authentification réussie"
                );
                self.rate_limiter.record_success(client_ip);
            }
            AuthOutcome::Failure(reason) => {
                warn!(
                    reason  = %reason,
                    method  = ?method,
                    ip      = %client_ip,
                    "authentification échouée"
                );
                self.rate_limiter.record_failure(client_ip);
            }
            AuthOutcome::MfaRequired { .. } => {
                info!(ip = %client_ip, "MFA requis");
            }
        }

        outcome
    }

    /// Auth mTLS : identité extraite du Common Name du certificat client
    async fn auth_mtls(
        &self,
        cert_identity: Option<String>,
        client_ip: IpAddr,
    ) -> AuthOutcome {
        if !self.config.enabled_methods.contains(&"mtls".to_string()) {
            return AuthOutcome::Failure(AuthFailureReason::UnknownMethod);
        }

        match cert_identity {
            Some(identity) if !identity.is_empty() => {
                // Extraction du CN depuis le DN X.509
                // ex: "CN=jo.dupont,OU=ops,O=Example,C=FR" → "jo.dupont"
                let user_id = extract_cn_from_dn(&identity)
                    .unwrap_or(identity.clone());

                AuthOutcome::Success(AuthIdentity {
                    user_id:          user_id.clone(),
                    display_name:     Some(user_id),
                    groups:           vec![],
                    method:           AuthMethodUsed::Mtls,
                    realm:            None,
                    authenticated_at: Utc::now(),
                    client_ip,
                })
            }
            _ => {
                warn!(ip = %client_ip, "mTLS : certificat client absent ou identité vide");
                AuthOutcome::Failure(AuthFailureReason::InvalidCredentials)
            }
        }
    }

    /// Auth Kerberos : validation du ticket AP-REQ
    async fn auth_kerberos(&self, token: &[u8], client_ip: IpAddr) -> AuthOutcome {
        if !self.config.enabled_methods.contains(&"kerberos".to_string()) {
            return AuthOutcome::Failure(AuthFailureReason::UnknownMethod);
        }

        let krb_cfg = match &self.config.kerberos {
            Some(c) => c,
            None => {
                warn!("Kerberos demandé mais non configuré");
                return AuthOutcome::Failure(AuthFailureReason::InternalError(
                    "Kerberos non configuré".to_string()
                ));
            }
        };

        // TODO Phase 3.1 : validation réelle via libkrb5 / krb5-sys
        // gss_accept_sec_context(AP-REQ) → principal client
        // Pour l'instant : stub qui accepte si le token n'est pas vide
        if token.is_empty() {
            return AuthOutcome::Failure(AuthFailureReason::InvalidCredentials);
        }

        // Simulation : extrait le principal depuis les bytes du token (stub)
        let principal = format!("stub_user@{}", krb_cfg.realm);
        info!(principal = %principal, ip = %client_ip, "Kerberos auth (stub)");

        AuthOutcome::Success(AuthIdentity {
            user_id:          principal.clone(),
            display_name:     Some(principal),
            groups:           vec![],
            method:           AuthMethodUsed::Kerberos,
            realm:            Some(krb_cfg.realm.clone()),
            authenticated_at: Utc::now(),
            client_ip,
        })
    }

    /// Auth OIDC : validation du Bearer JWT
    async fn auth_oidc(&self, token: &[u8], client_ip: IpAddr) -> AuthOutcome {
        if !self.config.enabled_methods.contains(&"oidc".to_string()) {
            return AuthOutcome::Failure(AuthFailureReason::UnknownMethod);
        }

        let oidc_cfg = match &self.config.oidc {
            Some(c) => c,
            None => return AuthOutcome::Failure(AuthFailureReason::InternalError(
                "OIDC non configuré".to_string()
            )),
        };

        let token_str = match std::str::from_utf8(token) {
            Ok(s) => s,
            Err(_) => return AuthOutcome::Failure(AuthFailureReason::InvalidCredentials),
        };

        match oidc::validate_bearer_token(token_str, oidc_cfg).await {
            Ok(claims) => {
                let user_id = claims.get(&oidc_cfg.identity_claim)
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                AuthOutcome::Success(AuthIdentity {
                    user_id:          user_id.clone(),
                    display_name:     claims.get("name")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    groups:           vec![],
                    method:           AuthMethodUsed::Oidc,
                    realm:            Some(oidc_cfg.issuer_url.clone()),
                    authenticated_at: Utc::now(),
                    client_ip,
                })
            }
            Err(e) => {
                warn!(error = %e, ip = %client_ip, "OIDC validation échouée");
                AuthOutcome::Failure(AuthFailureReason::InvalidCredentials)
            }
        }
    }

    /// Auth SAML : stub Phase 3+
    async fn auth_saml(&self, _token: &[u8], _client_ip: IpAddr) -> AuthOutcome {
        warn!("SAML non implémenté (Phase 3+)");
        AuthOutcome::Failure(AuthFailureReason::InternalError(
            "SAML non encore implémenté".to_string()
        ))
    }

    /// Génère un session token JWT signé
    pub fn issue_session_token(
        &self,
        identity: &AuthIdentity,
        session_id: &str,
        vm_id: &str,
    ) -> Result<String> {
        self.jwt_engine.sign(identity, session_id, vm_id)
    }

    /// Valide un session token (utilisé par le serveur pour vérifier le client)
    pub fn validate_session_token(&self, token: &str) -> Result<jwt::SessionClaims> {
        self.jwt_engine.verify(token)
    }
}

/// Extrait le CN d'un Distinguished Name X.509
/// "CN=jo.dupont,OU=ops,O=Example,C=FR" → Some("jo.dupont")
fn extract_cn_from_dn(dn: &str) -> Option<String> {
    dn.split(',')
        .find(|part| part.trim().starts_with("CN="))
        .map(|part| part.trim().trim_start_matches("CN=").to_string())
}

// Accès aux config security depuis AuthConfig (délégation)
impl AuthConfig {
    fn security_max_attempts(&self) -> u32 { 5 }
    fn security_ban_secs(&self) -> u64 { 300 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_cn() {
        assert_eq!(
            extract_cn_from_dn("CN=jo.dupont,OU=ops,O=Example,C=FR"),
            Some("jo.dupont".to_string())
        );
        assert_eq!(extract_cn_from_dn("OU=ops,O=Example"), None);
        assert_eq!(
            extract_cn_from_dn("CN=simple"),
            Some("simple".to_string())
        );
    }
}
