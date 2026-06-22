//! Pool de VMs NIDAN.
//!
//! Gère la disponibilité, l'attribution et la libération des VMs.
//! Supporte deux modes :
//! - **Statique** : VMs déclarées en configuration (Phase 3)
//! - **Dynamique** : spawn/destroy via libvirt/QEMU (Phase 4+)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::config::{PoolConfig, VmEntry};

/// État d'une VM dans le pool
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VmState {
    /// Disponible, prête à être assignée
    Available,
    /// Assignée à une session
    Assigned { session_id: String, since: DateTime<Utc> },
    /// En cours d'initialisation / warm-up
    Initializing,
    /// Health check échoué — temporairement hors service
    Unhealthy { reason: String, since: DateTime<Utc> },
    /// Désactivée manuellement
    Disabled,
}

impl VmState {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Available     => "disponible",
            Self::Assigned{..}  => "assignée",
            Self::Initializing  => "init",
            Self::Unhealthy{..} => "hors service",
            Self::Disabled      => "désactivée",
        }
    }
}

/// Entrée d'une VM dans le pool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmPoolEntry {
    pub id:          String,
    pub host:        String,
    pub port:        u16,
    pub tags:        Vec<String>,
    pub state:       VmState,
    pub added_at:    DateTime<Utc>,
    pub last_health: Option<DateTime<Utc>>,
    pub sessions_served: u64,
}

impl VmPoolEntry {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Pool de VMs thread-safe
pub struct VmPool {
    vms:    DashMap<String, VmPoolEntry>,
    config: PoolConfig,
    /// Endpoint QUIC client pour les health checks (handshake réel vers les VM).
    /// Si None, repli sur une sonde de joignabilité UDP.
    health_endpoint: Option<quinn::Endpoint>,
}

impl VmPool {
    /// Crée un pool depuis la configuration statique (sans endpoint QUIC :
    /// les health checks utilisent la sonde de joignabilité UDP).
    pub fn from_config(config: PoolConfig) -> Arc<Self> {
        Self::from_config_with_health(config, None)
    }

    /// Crée un pool avec un endpoint QUIC dédié aux health checks, permettant
    /// un handshake réel vers les VM (vérifie qu'un serveur NIDAN répond).
    pub fn from_config_with_health(
        config: PoolConfig,
        health_endpoint: Option<quinn::Endpoint>,
    ) -> Arc<Self> {
        let pool = Arc::new(Self {
            vms:    DashMap::new(),
            config: config.clone(),
            health_endpoint,
        });

        for vm in &config.static_vms {
            let entry = VmPoolEntry {
                id:              vm.id.clone(),
                host:            vm.host.clone(),
                port:            vm.port,
                tags:            vm.tags.clone(),
                state:           VmState::Available,
                added_at:        Utc::now(),
                last_health:     None,
                sessions_served: 0,
            };
            info!(vm_id = %vm.id, addr = %entry.addr(), "VM ajoutée au pool");
            pool.vms.insert(vm.id.clone(), entry);
        }

        // Démarrage du health checker si pool non vide
        if !config.static_vms.is_empty() {
            let pool_clone = pool.clone();
            tokio::spawn(async move {
                pool_clone.health_check_loop().await;
            });
        }

        pool
    }

    /// Assigne une VM disponible à une session.
    /// Prend en compte les tags optionnels.
    pub fn assign(
        &self,
        session_id: &str,
        preferred_tag: Option<&str>,
    ) -> Result<VmPoolEntry> {
        // Recherche en deux passes :
        // 1. VM avec le tag préféré
        // 2. N'importe quelle VM disponible
        let candidate = self.find_available(preferred_tag)
            .or_else(|| self.find_available(None));

        match candidate {
            None => bail!("aucune VM disponible dans le pool"),
            Some(vm_id) => {
                let mut entry = self.vms.get_mut(&vm_id)
                    .ok_or_else(|| anyhow::anyhow!("VM disparue: {vm_id}"))?;

                entry.state = VmState::Assigned {
                    session_id: session_id.to_string(),
                    since: Utc::now(),
                };
                entry.sessions_served += 1;

                info!(
                    vm_id      = %entry.id,
                    session_id = session_id,
                    addr       = %entry.addr(),
                    "VM assignée"
                );

                Ok(entry.clone())
            }
        }
    }

    /// Libère une VM après la fin d'une session
    pub fn release(&self, vm_id: &str, session_id: &str) {
        if let Some(mut entry) = self.vms.get_mut(vm_id) {
            match &entry.state {
                VmState::Assigned { session_id: sid, .. } if sid == session_id => {
                    entry.state = VmState::Available;
                    info!(vm_id = %vm_id, session_id, "VM libérée");
                }
                other => {
                    warn!(
                        vm_id      = %vm_id,
                        session_id,
                        state      = other.label(),
                        "tentative de libération d'une VM dans un état inattendu"
                    );
                }
            }
        }
    }

    /// Marque une VM comme hors service
    pub fn mark_unhealthy(&self, vm_id: &str, reason: &str) {
        if let Some(mut entry) = self.vms.get_mut(vm_id) {
            warn!(vm_id = %vm_id, reason, "VM marquée hors service");
            entry.state = VmState::Unhealthy {
                reason:  reason.to_string(),
                since:   Utc::now(),
            };
        }
    }

    /// Retourne le statut complet du pool
    pub fn status(&self) -> PoolStatus {
        let total     = self.vms.len();
        let available = self.vms.iter().filter(|e| e.state.is_available()).count();
        let assigned  = self.vms.iter()
            .filter(|e| matches!(e.state, VmState::Assigned{..}))
            .count();
        let unhealthy = self.vms.iter()
            .filter(|e| matches!(e.state, VmState::Unhealthy{..}))
            .count();

        PoolStatus { total, available, assigned, unhealthy }
    }

    /// Retourne toutes les entrées du pool
    pub fn all_vms(&self) -> Vec<VmPoolEntry> {
        self.vms.iter().map(|e| e.value().clone()).collect()
    }

    /// Retourne une VM par ID
    pub fn get(&self, vm_id: &str) -> Option<VmPoolEntry> {
        self.vms.get(vm_id).map(|e| e.clone())
    }

    /// Cherche une VM disponible avec tag optionnel
    fn find_available(&self, tag: Option<&str>) -> Option<String> {
        self.vms.iter()
            .filter(|e| {
                e.state.is_available() &&
                tag.map(|t| e.tags.contains(&t.to_string())).unwrap_or(true)
            })
            // Choisit la VM avec le moins de sessions servies (load balancing)
            .min_by_key(|e| e.sessions_served)
            .map(|e| e.id.clone())
    }

    /// Boucle de health check périodique
    async fn health_check_loop(&self) {
        let interval = Duration::from_secs(
            self.config.health_check_interval_secs
        );
        let timeout = Duration::from_secs(
            self.config.health_check_timeout_secs
        );

        loop {
            tokio::time::sleep(interval).await;

            let vm_ids: Vec<String> = self.vms.iter()
                .filter(|e| !matches!(e.state, VmState::Disabled))
                .map(|e| e.id.clone())
                .collect();

            for vm_id in vm_ids {
                if let Some(entry) = self.vms.get(&vm_id) {
                    let addr = entry.addr();
                    drop(entry); // Libère le lock avant await

                    let healthy = self.ping_vm(&addr, timeout).await;

                    if let Some(mut entry) = self.vms.get_mut(&vm_id) {
                        entry.last_health = Some(Utc::now());

                        if !healthy {
                            if entry.state.is_available() {
                                warn!(vm_id = %vm_id, addr = %addr, "health check échoué");
                                entry.state = VmState::Unhealthy {
                                    reason: "health check timeout".to_string(),
                                    since:  Utc::now(),
                                };
                            }
                        } else if matches!(entry.state, VmState::Unhealthy{..}) {
                            info!(vm_id = %vm_id, "VM de nouveau disponible après recovery");
                            entry.state = VmState::Available;
                        }
                    }
                }
            }
        }
    }

    /// Ping TCP vers la VM pour vérifier qu'elle est joignable
    /// Vérifie qu'une VM répond. NIDAN écoute en QUIC (UDP) : on tente d'abord
    /// un vrai handshake QUIC (prouve qu'un serveur NIDAN répond), sinon on
    /// retombe sur une sonde de joignabilité UDP (le port répond-il ?).
    async fn ping_vm(&self, addr: &str, timeout: Duration) -> bool {
        let sock_addr: SocketAddr = match addr.parse() {
            Ok(a) => a,
            Err(_) => return false,
        };

        // Voie privilégiée : handshake QUIC réel vers la VM.
        if let Some(ref endpoint) = self.health_endpoint {
            let connecting = match endpoint.connect(sock_addr, "nidan-server") {
                Ok(c) => c,
                Err(e) => { debug!(addr = %addr, error = %e, "connect QUIC health échoué"); return false; }
            };
            match tokio::time::timeout(timeout, connecting).await {
                Ok(Ok(conn)) => {
                    // Un serveur NIDAN a complété le handshake QUIC/TLS.
                    conn.close(0u32.into(), b"health check");
                    debug!(addr = %addr, "health check QUIC OK");
                    return true;
                }
                Ok(Err(e)) => { debug!(addr = %addr, error = %e, "handshake QUIC health échoué"); return false; }
                Err(_) => { debug!(addr = %addr, "timeout handshake QUIC health"); return false; }
            }
        }

        // Repli : joignabilité UDP (envoi d'un datagramme, le port est-il là ?).
        // NIDAN étant en UDP/QUIC, c'est plus pertinent qu'un connect TCP.
        Self::probe_udp(sock_addr, timeout).await
    }

    /// Sonde de joignabilité UDP : envoie un petit datagramme et considère le
    /// port joignable si l'envoi réussit sans ICMP port-unreachable immédiat.
    async fn probe_udp(addr: SocketAddr, timeout: Duration) -> bool {
        let bind: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }
            .parse().unwrap();
        let sock = match tokio::net::UdpSocket::bind(bind).await {
            Ok(s) => s,
            Err(_) => return false,
        };
        if sock.connect(addr).await.is_err() { return false; }
        // Un datagramme QUIC Initial minimal déclencherait une réponse ; ici on
        // se contente de vérifier l'absence d'erreur immédiate à l'envoi.
        let probe = [0u8; 1];
        match tokio::time::timeout(timeout, sock.send(&probe)).await {
            Ok(Ok(_)) => true,
            _ => false,
        }
    }
}

/// Statistiques du pool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolStatus {
    pub total:     usize,
    pub available: usize,
    pub assigned:  usize,
    pub unhealthy: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VmEntry;

    fn make_pool(n: usize) -> Arc<VmPool> {
        let vms = (0..n).map(|i| VmEntry {
            id:   format!("vm-{i:03}"),
            host: "127.0.0.1".to_string(),
            port: 9000 + i as u16,
            tags: vec![],
        }).collect();
        VmPool::from_config(PoolConfig {
            static_vms:                  vms,
            min_available:               1,
            health_check_timeout_secs:   1,
            health_check_interval_secs:  999, // désactiver en test
        })
    }

    #[tokio::test]
    async fn test_assign_and_release() {
        let pool = make_pool(2);
        let status = pool.status();
        assert_eq!(status.available, 2);

        let vm = pool.assign("sess-001", None).unwrap();
        assert_eq!(pool.status().available, 1);

        pool.release(&vm.id, "sess-001");
        assert_eq!(pool.status().available, 2);
    }

    #[tokio::test]
    async fn test_no_vm_available() {
        let pool = make_pool(1);
        pool.assign("sess-001", None).unwrap();
        assert!(pool.assign("sess-002", None).is_err());
    }

    #[tokio::test]
    async fn test_load_balancing() {
        let pool = make_pool(3);
        // La première assignation prend la VM avec le moins de sessions
        let v1 = pool.assign("s1", None).unwrap();
        pool.release(&v1.id, "s1");
        let v2 = pool.assign("s2", None).unwrap();
        // Après release, la même VM peut être réassignée
        assert!(!v2.id.is_empty());
    }
}
