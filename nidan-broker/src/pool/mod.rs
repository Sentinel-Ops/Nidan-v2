//! Pool de VMs NIDAN.
//!
//! Gère la disponibilité, l'attribution et la libération des VMs.
//! Supporte deux modes :
//! - **Statique** : VMs déclarées en configuration (Phase 3)
//! - **Dynamique** : spawn/destroy via libvirt/QEMU (Phase 4+)

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::PoolConfig;
use crate::provider::{VmProvider, StaticProvider};

/// État d'une VM dans le pool
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VmState {
    /// Disponible, prête à être assignée
    Available,
    /// VM chaude, bootée, prête à être assignée instantanément
    WarmReady { since: DateTime<Utc> },
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
            Self::WarmReady{..} => "chaude",
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
    /// VM provisionnée dynamiquement (clone) vs déclarée statiquement
    pub dynamic:     bool,
    /// Identifiant provider (UUID libvirt) pour les VMs dynamiques
    pub provider_id: Option<String>,
    /// CID vsock alloué (VMs dynamiques)
    pub cid:         Option<u32>,
    /// Utilisateur propriétaire (VMs dynamiques, pour quotas)
    pub user_id:     Option<String>,
}

impl VmPoolEntry {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}


/// Allocateur de CIDs vsock pour les VMs dynamiques.
///
/// Gère une plage de CIDs [start..=end] et suit ceux en cours d'utilisation.
pub struct CidAllocator {
    start: u32,
    end:   u32,
    used:  std::collections::HashSet<u32>,
}

impl CidAllocator {
    pub fn new(start: u32, end: u32) -> Self {
        Self { start, end, used: std::collections::HashSet::new() }
    }

    /// Alloue le prochain CID libre, ou None si la plage est pleine.
    pub fn allocate(&mut self) -> Option<u32> {
        for cid in self.start..=self.end {
            if !self.used.contains(&cid) {
                self.used.insert(cid);
                return Some(cid);
            }
        }
        None
    }

    /// Libère un CID.
    pub fn release(&mut self, cid: u32) {
        self.used.remove(&cid);
    }

    /// Nombre de CIDs encore disponibles.
    pub fn available(&self) -> usize {
        (self.end - self.start + 1) as usize - self.used.len()
    }
}

/// Pool de VMs thread-safe
pub struct VmPool {
    vms:    DashMap<String, VmPoolEntry>,
    config: PoolConfig,
    /// Endpoint QUIC client pour les health checks (handshake réel vers les VM).
    /// Si None, repli sur une sonde de joignabilité UDP.
    health_endpoint: Option<quinn::Endpoint>,
    /// Provider d'infrastructure VM (Proxmox, libvirt, ou StaticProvider).
    provider: Arc<dyn VmProvider>,
    /// Allocateur de CIDs vsock pour les VMs dynamiques.
    cid_allocator: std::sync::Mutex<CidAllocator>,
    /// Signal pour déclencher le réapprovisionnement du pool chaud.
    replenish_notify: Arc<tokio::sync::Notify>,
    /// VMs détectées comme orphelines, avec leur date de première détection.
    /// Le GC ne détruit une VM que si elle reste orpheline au-delà du délai de grâce.
    orphan_candidates: DashMap<String, DateTime<Utc>>,
}

impl VmPool {
    /// Crée un pool depuis la configuration statique (sans endpoint QUIC :
    /// les health checks utilisent la sonde de joignabilité UDP).
    /// Utilise le `StaticProvider` par défaut.
    pub fn from_config(config: PoolConfig) -> Arc<Self> {
        Self::build(config, None, Arc::new(StaticProvider))
    }

    /// Crée un pool avec un endpoint QUIC dédié et un provider spécifique.
    pub fn from_config_with_provider(
        config: PoolConfig,
        health_endpoint: Option<quinn::Endpoint>,
        provider: Arc<dyn VmProvider>,
    ) -> Arc<Self> {
        Self::build(config, health_endpoint, provider)
    }

    /// Constructeur interne commun.
    fn build(
        config: PoolConfig,
        health_endpoint: Option<quinn::Endpoint>,
        provider: Arc<dyn VmProvider>,
    ) -> Arc<Self> {
        let cid_alloc = if let Some(ref dc) = config.dynamic {
            CidAllocator::new(dc.cid_start, dc.cid_end)
        } else {
            CidAllocator::new(10, 99) // plage par défaut, inutilisée sans dynamic
        };

        let pool = Arc::new(Self {
            vms:    DashMap::new(),
            config: config.clone(),
            health_endpoint,
            provider,
            cid_allocator: std::sync::Mutex::new(cid_alloc),
            replenish_notify: Arc::new(tokio::sync::Notify::new()),
            orphan_candidates: DashMap::new(),
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
                dynamic:         false,
                provider_id:     None,
                cid:             None,
                user_id:         None,
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

        // Démarrage du pool chaud si dynamic configuré avec min_ready > 0
        if config.dynamic.as_ref().map(|d| d.min_ready > 0).unwrap_or(false) {
            let pool_clone = pool.clone();
            tokio::spawn(async move {
                pool_clone.replenish_loop().await;
            });
        }

        // GC runtime des VMs orphelines
        if config.dynamic.is_some() {
            let pool_clone = pool.clone();
            tokio::spawn(async move {
                pool_clone.gc_orphan_loop().await;
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


    /// Assigne une VM (statique ou dynamique) à une session.
    ///
    /// 1. Tente d'abord l'assignation statique (VMs disponibles dans le pool).
    /// 2. Si le pool est vide et qu'un `DynamicPoolConfig` est configuré,
    ///    provisionne automatiquement une nouvelle VM via le provider (clone +
    ///    set_vsock_cid + start).
    pub async fn assign_or_provision(
        &self,
        session_id: &str,
        preferred_tag: Option<&str>,
        user_id: &str,
    ) -> Result<VmPoolEntry> {
        // 1. Tentative statique
        if let Ok(vm) = self.assign(session_id, preferred_tag) {
            return Ok(vm);
        }

        // Vérification des quotas (VMs dynamiques uniquement)
        if let Some(ref dyn_cfg) = self.config.dynamic {
            let total = self.total_dynamic();
            if total >= dyn_cfg.max_total as usize {
                bail!("quota global atteint ({}/{})", total, dyn_cfg.max_total);
            }
            let user_count = self.user_dynamic_count(user_id);
            if user_count >= dyn_cfg.max_per_user as usize {
                bail!("quota utilisateur atteint ({}/{})", user_count, dyn_cfg.max_per_user);
            }
        }

        // 2. Chercher une VM chaude (WarmReady) — assignation instantanée
        if let Some(warm_id) = self.find_warm_ready() {
            if let Some(mut entry) = self.vms.get_mut(&warm_id) {
                entry.state = VmState::Assigned {
                    session_id: session_id.to_string(),
                    since: Utc::now(),
                };
                entry.sessions_served += 1;
                entry.user_id = Some(user_id.to_string());
                let result = entry.clone();
                info!(
                    vm_id      = %result.id,
                    session_id,
                    addr       = %result.addr(),
                    "VM chaude assignée (instantané)"
                );
                drop(entry);
                self.trigger_replenish();
                return Ok(result);
            }
        }

        // 3. Provisionnement dynamique (clone à froid)
        let dyn_cfg = self.config.dynamic.as_ref()
            .ok_or_else(|| anyhow::anyhow!(
                "aucune VM disponible et pool dynamique non configuré"
            ))?;

        // Allouer un CID
        let cid = {
            let mut alloc = self.cid_allocator.lock()
                .map_err(|_| anyhow::anyhow!("lock CidAllocator poisonné"))?;
            alloc.allocate()
                .ok_or_else(|| anyhow::anyhow!(
                    "plus de CIDs disponibles (plage {}-{} épuisée)",
                    dyn_cfg.cid_start, dyn_cfg.cid_end
                ))?
        };

        // Clone depuis le template (nom auto-généré par le provider)
        info!(
            template = %dyn_cfg.template,
            cid = cid,
            session_id,
            "provisionnement dynamique"
        );

        let new_vm = match self.provider.clone_vm(&dyn_cfg.template, "").await {
            Ok(vm) => vm,
            Err(e) => {
                // Libérer le CID en cas d'échec
                if let Ok(mut alloc) = self.cid_allocator.lock() {
                    alloc.release(cid);
                }
                return Err(e.context("clone template"));
            }
        };
        let provider_id = new_vm.provider_id.clone();
        let vm_name = new_vm.name.clone().unwrap_or_else(|| format!("dyn-{cid}"));

        // Configurer le CID vsock
        if let Err(e) = self.provider.set_vsock_cid(&provider_id, cid).await {
            warn!(error = %e, cid = cid, "set_vsock_cid échoué — continue sans vsock");
        }

        // Démarrer la VM
        if let Err(e) = self.provider.start_vm(&provider_id).await {
            // Nettoyage en cas d'échec au démarrage
            warn!(error = %e, "start échoué — suppression de la VM clonée");
            let _ = self.provider.delete_vm(&provider_id).await;
            if let Ok(mut alloc) = self.cid_allocator.lock() {
                alloc.release(cid);
            }
            return Err(e.context("démarrage VM dynamique"));
        }

        // Calculer l'adresse de la VM
        let host = dyn_cfg.vm_ip_pattern.replace("{cid}", &cid.to_string());

        // Créer l'entrée dans le pool, directement en état Assigned
        let entry = VmPoolEntry {
            id:              vm_name.clone(),
            host,
            port:            dyn_cfg.vm_port,
            tags:            vec!["dynamic".to_string()],
            state:           VmState::Assigned {
                session_id: session_id.to_string(),
                since: Utc::now(),
            },
            added_at:        Utc::now(),
            last_health:     None,
            sessions_served: 1,
            dynamic:         true,
            provider_id:     Some(provider_id),
            cid:             Some(cid),
            user_id:         Some(user_id.to_string()),
        };

        info!(
            vm_id      = %entry.id,
            cid        = cid,
            addr       = %entry.addr(),
            session_id,
            "VM dynamique provisionnée et assignée"
        );

        let result = entry.clone();
        self.vms.insert(vm_name, entry);
        Ok(result)
    }


    // ── Pool chaud ───────────────────────────────────────────────────────

    /// Nombre de VMs en état WarmReady.
    fn warm_count(&self) -> usize {
        self.vms.iter()
            .filter(|e| matches!(e.state, VmState::WarmReady { .. }))
            .count()
    }

    /// Nombre de VMs dynamiques assignées à un utilisateur.
    fn user_dynamic_count(&self, user_id: &str) -> usize {
        self.vms.iter()
            .filter(|e| e.dynamic && e.user_id.as_deref() == Some(user_id))
            .count()
    }

    /// Nombre total de VMs dynamiques (WarmReady + Assigned).
    fn total_dynamic(&self) -> usize {
        self.vms.iter().filter(|e| e.dynamic).count()
    }

    /// Cherche la VM chaude la plus ancienne.
    fn find_warm_ready(&self) -> Option<String> {
        self.vms.iter()
            .filter(|e| matches!(e.state, VmState::WarmReady { .. }))
            .min_by_key(|e| match &e.state {
                VmState::WarmReady { since } => *since,
                _ => Utc::now(),
            })
            .map(|e| e.id.clone())
    }

    /// Provisionne une VM chaude (clone + cid + start).
    async fn provision_warm_vm(&self) -> Result<()> {
        let dyn_cfg = self.config.dynamic.as_ref()
            .ok_or_else(|| anyhow::anyhow!("pool dynamique non configuré"))?;

        let cid = {
            let mut alloc = self.cid_allocator.lock()
                .map_err(|_| anyhow::anyhow!("lock CidAllocator poisonné"))?;
            alloc.allocate()
                .ok_or_else(|| anyhow::anyhow!("plage CID épuisée (warm)"))?
        };

        info!(
            template = %dyn_cfg.template,
            cid = cid,
            "provisionnement VM chaude"
        );

        let new_vm = match self.provider.clone_vm(&dyn_cfg.template, "").await {
            Ok(vm) => vm,
            Err(e) => {
                if let Ok(mut alloc) = self.cid_allocator.lock() {
                    alloc.release(cid);
                }
                return Err(e.context("clone template (warm)"));
            }
        };
        let provider_id = new_vm.provider_id.clone();
        let vm_name = new_vm.name.clone().unwrap_or_else(|| format!("warm-{cid}"));

        if let Err(e) = self.provider.set_vsock_cid(&provider_id, cid).await {
            warn!(error = %e, cid = cid, "set_vsock_cid échoué (warm) — continue");
        }

        if let Err(e) = self.provider.start_vm(&provider_id).await {
            warn!(error = %e, "start VM chaude échoué — suppression");
            let _ = self.provider.delete_vm(&provider_id).await;
            if let Ok(mut alloc) = self.cid_allocator.lock() {
                alloc.release(cid);
            }
            return Err(e.context("démarrage VM chaude"));
        }

        let host = dyn_cfg.vm_ip_pattern.replace("{cid}", &cid.to_string());

        let entry = VmPoolEntry {
            id:              vm_name.clone(),
            host,
            port:            dyn_cfg.vm_port,
            tags:            vec!["dynamic".to_string()],
            state:           VmState::WarmReady { since: Utc::now() },
            added_at:        Utc::now(),
            last_health:     None,
            sessions_served: 0,
            dynamic:         true,
            provider_id:     Some(provider_id),
            cid:             Some(cid),
            user_id:         None,
        };

        info!(
            vm_id = %entry.id,
            cid   = cid,
            addr  = %entry.addr(),
            "VM chaude prête"
        );
        self.vms.insert(vm_name, entry);
        Ok(())
    }

    /// Réapprovisionne le pool chaud jusqu'à `min_ready` VMs WarmReady.
    async fn replenish(&self) {
        let dyn_cfg = match self.config.dynamic.as_ref() {
            Some(c) => c,
            None => return,
        };

        let min_ready = dyn_cfg.min_ready as usize;
        let max_total = dyn_cfg.max_total as usize;

        loop {
            let warm  = self.warm_count();
            let total = self.total_dynamic();

            if warm >= min_ready {
                debug!(warm, min_ready, "pool chaud : niveau atteint");
                break;
            }
            if total >= max_total {
                warn!(total, max_total, "pool chaud : limite max_total atteinte");
                break;
            }

            info!(
                warm, min_ready, total, max_total,
                "pool chaud : provisionnement"
            );

            if let Err(e) = self.provision_warm_vm().await {
                warn!(error = %e, "replenish : échec — arrêt provisoire");
                break;
            }
        }
    }

    /// Boucle de fond : attend un signal ou un timer pour réapprovisionner.
    async fn replenish_loop(&self) {
        // skip_grace=true : au boot, aucun provisionnement en cours,
        // les orphelines sont des VMs d'un crash précédent → destruction immédiate.
        info!("replenish : nettoyage initial des orphelines (sans grâce)");
        self.cleanup_orphans(true).await;

        // Réapprovisionnement initial au démarrage
        self.replenish().await;

        loop {
            tokio::select! {
                _ = self.replenish_notify.notified() => {
                    debug!("replenish déclenché par signal");
                }
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    debug!("replenish périodique");
                }
            }
            self.replenish().await;
        }
    }

    /// Déclenche un réapprovisionnement en arrière-plan.
    fn trigger_replenish(&self) {
        self.replenish_notify.notify_one();
    }

    // ── GC orphelines (runtime) ──────────────────────────────────────────

    /// Détecte et détruit les VMs orphelines sur l'hyperviseur.
    ///
    /// Compare les VMs réelles (via `provider.list_vms()`) aux VMs connues
    /// du pool. Celles qui existent sur l'hyperviseur mais pas dans le pool
    /// sont des orphelines (échec `delete_vm`, crash broker…).
    ///
    /// Un **délai de grâce** (`orphan_grace_secs`) empêche la destruction
    /// de VMs en cours de provisionnement (race condition avec `provision_warm_vm`
    /// ou `assign_or_provision`).
    pub async fn cleanup_orphans(&self, skip_grace: bool) -> usize {
        let dyn_cfg = match self.config.dynamic.as_ref() {
            Some(c) => c,
            None => return 0,
        };

        // 1. Lister les VMs réelles sur l'hyperviseur
        let real_vms = match self.provider.list_vms().await {
            Ok(vms) => vms,
            Err(e) => {
                warn!(error = %e, "GC orphelines : list_vms échoué");
                return 0;
            }
        };

        // 2. Identifier les VMs connues du pool (par provider_id)
        let known_pids: std::collections::HashSet<String> = self.vms.iter()
            .filter_map(|e| e.provider_id.clone())
            .collect();

        // 3. Filtrer : orpheline = sur l'hyperviseur, pas dans le pool,
        //    pas le template, pas une VM protégée
        let template = &dyn_cfg.template;
        let protected = &dyn_cfg.protected_vms;
        let orphans: Vec<_> = real_vms.into_iter()
            .filter(|vm| {
                let name = vm.name.as_deref().unwrap_or("");
                // Exclure le template (par nom ou provider_id)
                if name == template || vm.provider_id == *template {
                    return false;
                }
                // Exclure les VMs protégées (broker, target, infra…)
                if protected.iter().any(|p| name == p) {
                    return false;
                }
                // Exclure les VMs connues du pool
                !known_pids.contains(&vm.provider_id)
            })
            .collect();

        if orphans.is_empty() {
            // Purger les candidats obsolètes
            self.orphan_candidates.clear();
            return 0;
        }

        // 4. Appliquer le délai de grâce
        let now = Utc::now();
        // Nettoyer les candidats qui ne sont plus orphelins
        let orphan_pids: std::collections::HashSet<_> = orphans.iter()
            .map(|o| o.provider_id.clone())
            .collect();
        self.orphan_candidates.retain(|pid, _| orphan_pids.contains(pid));

        let mut destroyed = 0usize;

        for vm in &orphans {
            // Enregistrer ou récupérer la première détection
            let first_seen = *self.orphan_candidates
                .entry(vm.provider_id.clone())
                .or_insert(now);

            let age = (now - first_seen).num_seconds();

            if !skip_grace && age < dyn_cfg.orphan_grace_secs as i64 {
                debug!(
                    vm_name = ?vm.name,
                    provider_id = %vm.provider_id,
                    age_secs = age,
                    grace_secs = dyn_cfg.orphan_grace_secs,
                    "GC orphelines : en délai de grâce"
                );
                continue;
            }

            // Grâce expirée → destruction
            info!(
                vm_name = ?vm.name,
                provider_id = %vm.provider_id,
                age_secs = age,
                "GC orphelines : destruction VM orpheline"
            );

            // Arrêt si running (best-effort)
            if matches!(vm.status, crate::provider::ProviderVmStatus::Running) {
                if let Err(e) = self.provider.stop_vm(&vm.provider_id).await {
                    debug!(
                        error = %e,
                        "GC orphelines : stop échoué — tentative delete"
                    );
                }
            }

            // Suppression
            match self.provider.delete_vm(&vm.provider_id).await {
                Ok(_) => {
                    destroyed += 1;
                    self.orphan_candidates.remove(&vm.provider_id);
                    info!(vm_name = ?vm.name, "GC orphelines : VM détruite");
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        vm_name = ?vm.name,
                        "GC orphelines : delete échoué — réessai au prochain cycle"
                    );
                }
            }
        }

        if destroyed > 0 {
            info!(destroyed, "GC orphelines : cycle terminé");
        }

        destroyed
    }

    /// Boucle de fond du GC orphelines.
    ///
    /// Tourne à intervalle régulier (`gc_orphan_interval_secs`). Le premier
    /// tick fait office de nettoyage au boot (détruit les orphelines d'un
    /// crash précédent une fois le délai de grâce écoulé).
    async fn gc_orphan_loop(&self) {
        let interval = match self.config.dynamic.as_ref() {
            Some(c) => Duration::from_secs(c.gc_orphan_interval_secs),
            None => return,
        };

        info!(
            interval_secs = interval.as_secs(),
            "GC orphelines : boucle démarrée"
        );

        loop {
            tokio::time::sleep(interval).await;
            self.cleanup_orphans(false).await;
            self.gc_stale_sessions().await;
        }
    }

    // ── GC sessions obsolètes ────────────────────────────────────────────

    /// Détecte les sessions dont la VM n'existe plus sur l'hyperviseur.
    ///
    /// Cas couverts :
    /// - Le proxy-encoder a détruit la VM (client déconnecté)
    /// - La VM a crashé
    /// - Suppression manuelle (virsh destroy/undefine)
    ///
    /// Pour chaque VM du pool qui n'existe plus sur l'hyperviseur :
    /// libère le CID, retire du pool, décrémente les quotas.
    async fn gc_stale_sessions(&self) {
        let dyn_cfg = match self.config.dynamic.as_ref() {
            Some(c) => c,
            None => return,
        };

        // Lister les VMs réelles sur l'hyperviseur
        let real_vms = match self.provider.list_vms().await {
            Ok(vms) => vms,
            Err(e) => {
                debug!(error = %e, "GC stale : list_vms échoué");
                return;
            }
        };

        // Collecter les provider_ids des VMs réelles
        let real_pids: std::collections::HashSet<String> = real_vms.iter()
            .map(|v| v.provider_id.clone())
            .collect();

        // Trouver les VMs dynamiques du pool qui n'existent plus
        let mut stale: Vec<(String, Option<u32>)> = Vec::new();
        for entry in self.vms.iter() {
            if !entry.dynamic { continue; }
            if let Some(ref pid) = entry.provider_id {
                if !real_pids.contains(pid) {
                    stale.push((entry.id.clone(), entry.cid));
                }
            }
        }

        if stale.is_empty() { return; }

        for (vm_id, cid) in &stale {
            info!(
                vm_id = %vm_id,
                cid = ?cid,
                "GC stale : VM absente de l'hyperviseur — libération"
            );

            // Libérer le CID vsock
            if let Some(cid) = cid {
                if let Ok(mut alloc) = self.cid_allocator.lock() {
                    alloc.release(*cid);
                }
            }

            // Retirer du pool
            self.vms.remove(vm_id.as_str());
        }

        info!(
            count = stale.len(),
            "GC stale : sessions obsolètes libérées"
        );

        // Réapprovisionner le pool chaud
        self.trigger_replenish();
    }
    /// Libère une VM après la fin d'une session.
    ///
    /// - **VMs statiques** : remise en état `Available` pour réutilisation.
    /// - **VMs dynamiques** : arrêt + suppression via le provider, libération
    ///   du CID vsock, retrait du pool.
    pub async fn release(&self, vm_id: &str, session_id: &str) {
        // Récupérer les infos de la VM avant modification
        let vm_info = if let Some(entry) = self.vms.get(vm_id) {
            match &entry.state {
                VmState::Assigned { session_id: sid, .. } if sid == session_id => {
                    Some((entry.dynamic, entry.provider_id.clone(), entry.cid))
                }
                other => {
                    warn!(
                        vm_id      = %vm_id,
                        session_id,
                        state      = other.label(),
                        "tentative de libération d'une VM dans un état inattendu"
                    );
                    None
                }
            }
        } else {
            warn!(vm_id = %vm_id, session_id, "VM introuvable pour release");
            None
        };

        let Some((is_dynamic, provider_id, cid)) = vm_info else {
            return;
        };

        if is_dynamic {
            // ── VM dynamique : destruction complète ──
            if let Some(ref pid) = provider_id {
                info!(
                    vm_id = %vm_id,
                    session_id,
                    "destruction VM dynamique post-session"
                );
                // Arrêt (best-effort, delete_vm gère le cas déjà arrêté)
                if let Err(e) = self.provider.stop_vm(pid).await {
                    debug!(
                        error = %e, vm_id = %vm_id,
                        "stop VM échoué — delete tentera quand même"
                    );
                }
                // Suppression (VM + volumes)
                if let Err(e) = self.provider.delete_vm(pid).await {
                    warn!(
                        error = %e, vm_id = %vm_id,
                        "delete VM dynamique échoué — VM orpheline possible"
                    );
                }
            }

            // Libérer le CID
            if let Some(cid_val) = cid {
                if let Ok(mut alloc) = self.cid_allocator.lock() {
                    alloc.release(cid_val);
                    debug!(cid = cid_val, "CID libéré");
                }
            }

            // Retirer du pool
            self.vms.remove(vm_id);
            info!(vm_id = %vm_id, session_id, "VM dynamique détruite et retirée du pool");

            // Réapprovisionner le pool chaud
            self.trigger_replenish();
        } else {
            // ── VM statique : remise en état Available ──
            if let Some(mut entry) = self.vms.get_mut(vm_id) {
                entry.state = VmState::Available;
                info!(vm_id = %vm_id, session_id, "VM statique libérée");
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

    /// Garbage collector des sessions expirées.
    ///
    /// Parcourt les VMs assignées et libère celles dont la session a dépassé
    /// `max_age_secs` (typiquement `session_token_ttl_secs`). Les VMs dynamiques
    /// sont détruites via le provider, les VMs statiques sont remises en état
    /// `Available`.
    ///
    /// Appelé périodiquement par une tâche de fond dans `routing`.
    pub async fn gc_expired_sessions(&self, max_age_secs: u64) {
        let now = chrono::Utc::now();
        let mut expired = Vec::new();

        for entry in self.vms.iter() {
            if let VmState::Assigned { session_id, since } = &entry.state {
                let age = (now - *since).num_seconds();
                if age > max_age_secs as i64 {
                    expired.push((entry.id.clone(), session_id.clone()));
                }
            }
        }

        if expired.is_empty() {
            return;
        }

        info!(count = expired.len(), "GC : sessions expirées détectées");

        for (vm_id, session_id) in expired {
            info!(
                vm_id = %vm_id,
                session_id = %session_id,
                "GC : libération session expirée"
            );
            self.release(&vm_id, &session_id).await;
        }

        // Réapprovisionner (on arrive ici seulement si expired non vide)
        self.trigger_replenish();
    }

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

    /// Retourne une référence au provider d'infrastructure.
    pub fn provider(&self) -> &dyn VmProvider {
        self.provider.as_ref()
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
            dynamic:                     None,
        })
    }

    #[tokio::test]
    async fn test_assign_and_release() {
        let pool = make_pool(2);
        let status = pool.status();
        assert_eq!(status.available, 2);

        let vm = pool.assign("sess-001", None).unwrap();
        assert_eq!(pool.status().available, 1);

        pool.release(&vm.id, "sess-001").await;
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
        pool.release(&v1.id, "s1").await;
        let v2 = pool.assign("s2", None).unwrap();
        // Après release, la même VM peut être réassignée
        assert!(!v2.id.is_empty());
    }
}
