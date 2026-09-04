//! Abstraction des fournisseurs d'infrastructure VM.
//!
//! Le trait [`VmProvider`] définit les opérations que le broker attend
//! d'un hyperviseur : lister, cloner, démarrer, arrêter, supprimer.
//! Chaque backend (Proxmox, libvirt…) implémente ce trait.
//!
//! En mode statique pur (VMs déclarées en configuration), le
//! [`StaticProvider`] est utilisé — il refuse toutes les opérations
//! dynamiques avec un message explicite.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Identité d'une VM telle que vue par le provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderVm {
    /// Identifiant unique côté provider (ex: VMID Proxmox, UUID libvirt)
    pub provider_id: String,
    /// Nom lisible
    pub name: Option<String>,
    /// État courant
    pub status: ProviderVmStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderVmStatus {
    Running,
    Stopped,
    Unknown(String),
}

/// Trait d'abstraction d'un fournisseur de VMs.
///
/// Toutes les méthodes sont `&self` — le provider est partagé
/// (via `Arc<dyn VmProvider>`) entre le pool et les health checks.
#[async_trait]
pub trait VmProvider: Send + Sync + 'static {
    /// Nom du backend pour les logs (ex: "proxmox", "libvirt").
    fn backend_name(&self) -> &'static str;

    /// Liste les VMs gérées par ce provider.
    async fn list_vms(&self) -> Result<Vec<ProviderVm>>;

    /// Récupère le statut d'une VM.
    async fn get_status(&self, provider_id: &str) -> Result<ProviderVm>;

    /// Clone un template en une nouvelle VM.
    async fn clone_vm(
        &self,
        template_id: &str,
        new_name: &str,
    ) -> Result<ProviderVm>;

    /// Démarre une VM arrêtée.
    async fn start_vm(&self, provider_id: &str) -> Result<()>;

    /// Arrête une VM.
    async fn stop_vm(&self, provider_id: &str) -> Result<()>;

    /// Détruit une VM (irréversible).
    async fn delete_vm(&self, provider_id: &str) -> Result<()>;

    /// Configure le CID vsock d'une VM (spécifique NIDAN).
    async fn set_vsock_cid(&self, provider_id: &str, cid: u32) -> Result<()>;
}

/// Provider "noop" pour le mode pool statique pur.
///
/// Toutes les opérations dynamiques retournent une erreur explicite.
/// C'est le provider par défaut quand aucun backend n'est configuré.
pub struct StaticProvider;

#[async_trait]
impl VmProvider for StaticProvider {
    fn backend_name(&self) -> &'static str { "static" }

    async fn list_vms(&self) -> Result<Vec<ProviderVm>> {
        Ok(vec![])
    }

    async fn get_status(&self, provider_id: &str) -> Result<ProviderVm> {
        anyhow::bail!(
            "StaticProvider: pas de statut dynamique pour {provider_id} \
             — configurer un backend (proxmox, libvirt) pour les opérations dynamiques"
        )
    }

    async fn clone_vm(
        &self,
        _template_id: &str,
        _new_name: &str,
    ) -> Result<ProviderVm> {
        anyhow::bail!("StaticProvider: clonage non supporté en mode statique")
    }

    async fn start_vm(&self, _id: &str) -> Result<()> {
        anyhow::bail!("StaticProvider: start non supporté en mode statique")
    }

    async fn stop_vm(&self, _id: &str) -> Result<()> {
        anyhow::bail!("StaticProvider: stop non supporté en mode statique")
    }

    async fn delete_vm(&self, _id: &str) -> Result<()> {
        anyhow::bail!("StaticProvider: delete non supporté en mode statique")
    }

    async fn set_vsock_cid(&self, _id: &str, _cid: u32) -> Result<()> {
        anyhow::bail!("StaticProvider: set_vsock_cid non supporté en mode statique")
    }
}


// ── Factory ─────────────────────────────────────────────────────────────────

/// Construit le provider d'infrastructure VM à partir de la configuration.
///
/// Appelé au démarrage du broker dans `routing::BrokerState::new()`.
pub fn build_provider(
    config: &crate::config::ProviderConfig,
) -> anyhow::Result<std::sync::Arc<dyn VmProvider>> {
    use std::sync::Arc;

    match config.backend.as_str() {
        "static" => {
            tracing::info!("provider: mode statique (VMs déclarées en config)");
            Ok(Arc::new(StaticProvider))
        }

        #[cfg(feature = "provider-proxmox")]
        "proxmox" => {
            // La migration de la config Proxmox vers [provider.proxmox]
            // sera finalisée en Phase 4. Pour l'instant, le ProxmoxClient
            // se construit depuis ProxmoxConfig (provider/proxmox.rs).
            anyhow::bail!(
                "provider proxmox: la configuration [provider.proxmox] n'est pas encore \
                 supportée — utiliser le mode statique ou libvirt"
            )
        }

        #[cfg(feature = "provider-libvirt")]
        "libvirt" => {
            let lv_cfg = config.libvirt.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "provider libvirt: section [provider.libvirt] requise dans la config"
                )
            })?;
            let provider = libvirt::LibvirtProvider::new(lv_cfg)?;
            Ok(Arc::new(provider))
        }

        #[cfg(feature = "provider-host-agent")]
        "host-agent" => {
            let ha_cfg = config.host_agent.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "provider host-agent: section [provider.host_agent] requise"
                )
            })?;
            let provider = host_agent::HostAgentProvider::new(ha_cfg)?;
            Ok(Arc::new(provider))
        }

        other => {
            let mut supported = vec!["static"];
            #[cfg(feature = "provider-proxmox")]
            supported.push("proxmox");
            #[cfg(feature = "provider-libvirt")]
            supported.push("libvirt");
            #[cfg(feature = "provider-host-agent")]
            supported.push("host-agent");
            anyhow::bail!(
                "provider.backend inconnu: \"{other}\" (supportés: {})",
                supported.join(", ")
            )
        }
    }
}

// ── Sous-modules conditionnels ──────────────────────────────────────────────

#[cfg(feature = "provider-proxmox")]
pub mod proxmox;

#[cfg(feature = "provider-libvirt")]
pub mod libvirt;

#[cfg(feature = "provider-host-agent")]
pub mod host_agent;
