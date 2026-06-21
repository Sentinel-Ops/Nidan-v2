//! Serveur QUIC du broker et orchestration des sessions.
//!
//! Gère le cycle complet : connexion client → auth → pool → routing.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use tokio::sync::RwLock;
use tracing::{error, info, warn};

use nidan_common::session::SessionId;
use nidan_proto::{AuthMethod, BrokerSessionResponse, ClientSessionRequest};

use crate::auth::{AuthEngine, AuthOutcome};
use crate::config::BrokerConfig;
use crate::pool::VmPool;
use crate::session::{BrokerSession, BrokerSessionState, SessionRegistry};

/// État partagé du broker (Arc-wrappé pour partage entre tâches)
#[derive(Clone)]
pub struct BrokerState {
    pub config:   BrokerConfig,
    pub pool:     Arc<VmPool>,
    pub sessions: Arc<SessionRegistry>,
    pub auth:     Arc<AuthEngine>,
}

impl BrokerState {
    pub async fn new(config: BrokerConfig) -> Result<Arc<Self>> {
        let pool     = VmPool::from_config(config.pool.clone());
        let sessions = SessionRegistry::new();
        let auth     = Arc::new(AuthEngine::new(config.auth.clone()));

        let status = pool.status();
        info!(
            total     = status.total,
            available = status.available,
            "pool de VMs initialisé"
        );

        Ok(Arc::new(Self { config, pool, sessions, auth }))
    }
}

/// Démarre le serveur QUIC du broker
pub async fn run_quic_server(state: Arc<BrokerState>) -> Result<()> {
    let bind_addr: SocketAddr = state.config.network.quic_bind
        .parse()
        .with_context(|| format!("adresse QUIC invalide: {}", state.config.network.quic_bind))?;

    let tls_config = build_server_tls(&state.config)
        .context("configuration TLS broker")?;

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
    let endpoint = quinn::Endpoint::server(server_config, bind_addr)
        .with_context(|| format!("bind QUIC sur {}", bind_addr))?;

    info!(addr = %bind_addr, "broker QUIC en écoute");

    let shutdown = tokio_util::sync::CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        shutdown_clone.cancel();
    });

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("arrêt broker QUIC");
                endpoint.close(0u32.into(), b"broker shutdown");
                break;
            }
            incoming = endpoint.accept() => {
                match incoming {
                    None => break,
                    Some(conn) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(conn, state).await {
                                warn!(error = %e, "erreur connexion client");
                            }
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

/// Gère une connexion client entrante
async fn handle_client(
    incoming: quinn::Incoming,
    state: Arc<BrokerState>,
) -> Result<()> {
    let conn = incoming.await.context("acceptation connexion")?;
    let remote_ip = conn.remote_address().ip();

    info!(remote = %conn.remote_address(), "nouveau client broker");

    // Vérification du nombre max de sessions
    if state.sessions.active_count() >= state.config.network.max_sessions {
        warn!(ip = %remote_ip, "limite de sessions atteinte — refus");
        conn.close(1u32.into(), b"max sessions reached");
        return Ok(());
    }

    // Réception de la demande de session
    let (request, mut resp_tx) = receive_session_request(&conn).await
        .context("réception session request")?;

    // Extraction de l'identité mTLS depuis le certificat TLS
    let cert_identity = crate::auth::mtls::extract_peer_identity(&conn);
    if let Some(ref dn) = cert_identity {
        info!(client_dn = %dn, "identité mTLS extraite du certificat client");
    } else {
        info!("aucune identité mTLS (connexion sans certificat client)");
    }

    // Authentification
    let auth_method = AuthMethod::try_from(request.auth_method)
        .unwrap_or(AuthMethod::Unspecified);

    let outcome = state.auth.authenticate(
        auth_method,
        &request.auth_token,
        remote_ip,
        cert_identity,
    ).await;

    match outcome {
        AuthOutcome::Failure(reason) => {
            warn!(reason = %reason, ip = %remote_ip, "auth refusée");
            let resp = BrokerSessionResponse {
                auth_result:   nidan_proto::AuthResult::Failure as i32,
                error_message: reason.to_string(),
                ..Default::default()
            };
            send_session_response(&mut resp_tx, &resp).await?;
            conn.close(2u32.into(), b"auth failed");
            return Ok(());
        }

        AuthOutcome::MfaRequired { challenge } => {
            let resp = BrokerSessionResponse {
                auth_result:   nidan_proto::AuthResult::MfaNeeded as i32,
                error_message: challenge,
                ..Default::default()
            };
            send_session_response(&mut resp_tx, &resp).await?;
            return Ok(());
        }

        AuthOutcome::Success(identity) => {
            // Attribution d'une VM
            let session_id = SessionId::new();
            let preferred_tag = if request.preferred_vm_tag.is_empty() {
                None
            } else {
                Some(request.preferred_vm_tag.as_str())
            };

            let vm = match state.pool.assign(session_id.as_ref(), preferred_tag) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, user = %identity.user_id, "pas de VM disponible");
                    let resp = BrokerSessionResponse {
                        auth_result:   nidan_proto::AuthResult::Failure as i32,
                        error_message: "aucune VM disponible".to_string(),
                        ..Default::default()
                    };
                    send_session_response(&mut resp_tx, &resp).await?;
                    return Ok(());
                }
            };

            // Génération du session token JWT
            let token = state.auth.issue_session_token(
                &identity,
                session_id.as_ref(),
                &vm.id,
            ).context("génération session token")?;

            // Le broker ne participe PAS à l'échange de clés E2E : le client
            // négocie X25519 directement avec le serveur (broker opaque au contenu).
            // Les champs crypto restent vides côté broker, par conception.
            let resp = BrokerSessionResponse {
                auth_result:    nidan_proto::AuthResult::Success as i32,
                session_id:     session_id.to_string(),
                vm_id:          vm.id.clone(),
                server_address: vm.addr(),
                session_token:  token.into_bytes(),
                server_nonce:      vec![],
                server_public_key: vec![],
                ..Default::default()
            };

            // Enregistrement de la session
            state.sessions.register(BrokerSession {
                id:          session_id.to_string(),
                user_id:     identity.user_id.clone(),
                client_ip:   remote_ip.to_string(),
                vm_id:       vm.id.clone(),
                vm_addr:     vm.addr(),
                state:       BrokerSessionState::Active,
                auth_method: identity.method.to_string(),
                started_at:  chrono::Utc::now(),
                last_seen:   chrono::Utc::now(),
            });

            send_session_response(&mut resp_tx, &resp).await?;

            info!(
                session_id = %session_id,
                user       = %identity.user_id,
                vm         = %vm.id,
                "session broker établie"
            );

            // Surveillance de la session jusqu'à déconnexion
            let vm_id_clone = vm.id.clone();
            let session_id_str = session_id.to_string();
            let state_clone = state.clone();

            tokio::spawn(async move {
                let timeout = state_clone.config.network.session_timeout_secs;
                tokio::select! {
                    _ = conn.closed() => {}
                    _ = tokio::time::sleep(
                        tokio::time::Duration::from_secs(timeout)
                    ) => {
                        warn!(session_id = %session_id_str, "timeout session — fermeture");
                        conn.close(3u32.into(), b"session timeout");
                    }
                }
                state_clone.pool.release(&vm_id_clone, &session_id_str);
                state_clone.sessions.close(&session_id_str);
                info!(session_id = %session_id_str, "session broker fermée");
            });
        }
    }

    Ok(())
}

/// Lit une ClientSessionRequest length-prefixed
async fn receive_session_request(
    conn: &quinn::Connection,
) -> Result<(ClientSessionRequest, quinn::SendStream)> {
    // Le client ouvre UN stream bi : il envoie la requête puis attend la
    // réponse sur le MÊME stream. On conserve donc la moitié SEND pour répondre.
    let (tx, mut rx) = conn.accept_bi().await.context("stream session request")?;

    let mut len_buf = [0u8; 4];
    rx.read_exact(&mut len_buf).await.context("lecture longueur")?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > 8192 { bail!("session request trop grand: {len}"); }

    let mut buf = vec![0u8; len];
    rx.read_exact(&mut buf).await.context("lecture payload")?;

    let req = nidan_proto::decode_message::<ClientSessionRequest>(&buf).context("décodage proto")?;
    Ok((req, tx))
}

/// Envoie une BrokerSessionResponse
async fn send_session_response(
    tx: &mut quinn::SendStream,
    resp: &BrokerSessionResponse,
) -> Result<()> {
    // Sérialisation brute + préfixe longueur unique (le client lit [len][json])
    let data = serde_json::to_vec(resp).context("sérialisation réponse")?;
    tx.write_all(&(data.len() as u32).to_be_bytes()).await?;
    tx.write_all(&data).await?;
    tx.finish().context("flush réponse")?;
    Ok(())
}

/// Configuration TLS du broker
fn build_server_tls(config: &BrokerConfig) -> Result<quinn::crypto::rustls::QuicServerConfig> {
    use std::fs;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rustls_pemfile::{certs as parse_certs, pkcs8_private_keys};

    let cert_pem = fs::read(&config.tls.cert)?;
    let broker_certs: Vec<CertificateDer> = parse_certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()?;

    let key_pem = fs::read(&config.tls.key)?;
    let mut keys = pkcs8_private_keys(&mut key_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()?;
    if keys.is_empty() { anyhow::bail!("aucune clé PKCS8"); }

    let ca_pem = fs::read(&config.tls.ca_cert)?;
    let ca_certs: Vec<CertificateDer> = parse_certs(&mut ca_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()?;

    let mut root_store = rustls::RootCertStore::empty();
    for ca in ca_certs { root_store.add(ca)?; }

    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()?;

    let rustls_cfg = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(broker_certs, PrivateKeyDer::Pkcs8(keys.remove(0)))?;

    quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
        .context("QuicServerConfig broker")
}
