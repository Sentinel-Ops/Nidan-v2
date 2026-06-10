//! Registre des sessions actives côté broker.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::info;

use nidan_common::session::SessionId;

/// État d'une session broker
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BrokerSessionState {
    Authenticating,
    Active,
    Closing,
    Closed,
}

/// Session active enregistrée dans le broker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerSession {
    pub id:          String,
    pub user_id:     String,
    pub client_ip:   String,
    pub vm_id:       String,
    pub vm_addr:     String,
    pub state:       BrokerSessionState,
    pub auth_method: String,
    pub started_at:  DateTime<Utc>,
    pub last_seen:   DateTime<Utc>,
}

/// Registre des sessions thread-safe
pub struct SessionRegistry {
    sessions: DashMap<String, BrokerSession>,
}

impl SessionRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { sessions: DashMap::new() })
    }

    pub fn register(&self, session: BrokerSession) {
        info!(
            session_id = %session.id,
            user_id    = %session.user_id,
            vm_id      = %session.vm_id,
            "session enregistrée"
        );
        self.sessions.insert(session.id.clone(), session);
    }

    pub fn close(&self, session_id: &str) -> Option<BrokerSession> {
        if let Some(mut s) = self.sessions.get_mut(session_id) {
            s.state = BrokerSessionState::Closed;
        }
        info!(session_id, "session fermée");
        self.sessions.remove(session_id).map(|(_, s)| s)
    }

    pub fn get(&self, session_id: &str) -> Option<BrokerSession> {
        self.sessions.get(session_id).map(|s| s.clone())
    }

    pub fn all(&self) -> Vec<BrokerSession> {
        self.sessions.iter().map(|s| s.clone()).collect()
    }

    pub fn active_count(&self) -> usize {
        self.sessions.iter()
            .filter(|s| s.state == BrokerSessionState::Active)
            .count()
    }

    /// Revoke forcée d'une session (admin)
    pub fn revoke(&self, session_id: &str, reason: &str) -> bool {
        if let Some(mut s) = self.sessions.get_mut(session_id) {
            s.state = BrokerSessionState::Closing;
            info!(session_id, reason, "session révoquée par admin");
            true
        } else {
            false
        }
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self { sessions: DashMap::new() }
    }
}
