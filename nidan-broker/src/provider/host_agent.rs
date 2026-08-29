//! Provider host-agent : pilotage des VMs via le nidan-host-agent (vsock).
//!
//! Ce provider implémente [`VmProvider`] en communiquant avec le
//! `nidan-host-agent` sur le socle via un canal vsock. Il n'appelle
//! jamais libvirt directement — toutes les opérations sont relayées
//! par l'agent, qui applique ses propres contrôles de sécurité.
//!
//! Compilé uniquement avec `--features provider-host-agent`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_vsock::{VsockAddr, VsockStream};
use tracing::{debug, info};

use nidan_proto::host_agent::{AgentRequest, AgentResponse, AgentVm};

use super::{ProviderVm, ProviderVmStatus, VmProvider};
use crate::config::HostAgentProviderConfig;

/// Provider qui délègue toutes les opérations VM au nidan-host-agent via vsock.
pub struct HostAgentProvider {
    config: HostAgentProviderConfig,
}

impl HostAgentProvider {
    /// Crée le provider. La connectivité vsock est vérifiée à la première
    /// requête (pas au démarrage, car le module vsock peut ne pas être
    /// chargé pendant les tests unitaires).
    pub fn new(config: &HostAgentProviderConfig) -> Result<Self> {
        info!(
            host_cid = config.host_cid,
            port = config.port,
            "provider host-agent configuré"
        );
        Ok(Self {
            config: config.clone(),
        })
    }

    /// Envoie une requête au host-agent et retourne la réponse.
    ///
    /// Ouvre une connexion vsock par requête (pas de connexion persistante).
    /// Framing : [4 bytes BE longueur][JSON payload].
    async fn send_request(&self, req: &AgentRequest) -> Result<AgentResponse> {
        let addr = VsockAddr::new(self.config.host_cid, self.config.port);

        let mut stream = VsockStream::connect(addr)
            .await
            .with_context(|| format!(
                "connexion vsock {}:{}", self.config.host_cid, self.config.port
            ))?;

        // Envoyer la requête
        let payload = serde_json::to_vec(req)
            .context("sérialisation requête agent")?;
        let len_bytes = (payload.len() as u32).to_be_bytes();

        stream.write_all(&len_bytes).await?;
        stream.write_all(&payload).await?;

        debug!(action = ?std::mem::discriminant(req), "requête envoyée au host-agent");

        // Lire la réponse
        let mut resp_len = [0u8; 4];
        stream.read_exact(&mut resp_len)
            .await
            .context("lecture longueur réponse agent")?;
        let resp_size = u32::from_be_bytes(resp_len) as usize;

        if resp_size > 1024 * 1024 {
            anyhow::bail!("réponse agent trop grande: {resp_size} bytes");
        }

        let mut resp_buf = vec![0u8; resp_size];
        stream.read_exact(&mut resp_buf)
            .await
            .context("lecture payload réponse agent")?;

        let response: AgentResponse = serde_json::from_slice(&resp_buf)
            .context("désérialisation réponse agent")?;

        Ok(response)
    }
}

/// Convertit un AgentVm (types proto) en ProviderVm (types broker).
fn agent_vm_to_provider(vm: AgentVm) -> ProviderVm {
    let status = match vm.status.as_str() {
        "running" => ProviderVmStatus::Running,
        "stopped" => ProviderVmStatus::Stopped,
        other     => ProviderVmStatus::Unknown(other.to_string()),
    };
    ProviderVm {
        provider_id: vm.provider_id,
        name: Some(vm.name),
        status,
    }
}

#[async_trait]
impl VmProvider for HostAgentProvider {
    fn backend_name(&self) -> &'static str { "host-agent" }

    async fn list_vms(&self) -> Result<Vec<ProviderVm>> {
        let req = AgentRequest::ListVms {
            prefix: self.config.vm_prefix.clone(),
        };
        let resp = self.send_request(&req).await?;
        let vms: Vec<AgentVm> = resp.into_result()
            .map_err(|e| anyhow::anyhow!("host-agent list_vms: {e}"))?;
        Ok(vms.into_iter().map(agent_vm_to_provider).collect())
    }

    async fn get_status(&self, provider_id: &str) -> Result<ProviderVm> {
        let req = AgentRequest::GetStatus {
            vm_id: provider_id.to_string(),
        };
        let resp = self.send_request(&req).await?;
        let vm: AgentVm = resp.into_result()
            .map_err(|e| anyhow::anyhow!("host-agent get_status: {e}"))?;
        Ok(agent_vm_to_provider(vm))
    }

    async fn clone_vm(
        &self,
        template_id: &str,
        new_name: &str,
    ) -> Result<ProviderVm> {
        let req = AgentRequest::CloneVm {
            template: template_id.to_string(),
            new_name: new_name.to_string(),
        };
        let resp = self.send_request(&req).await?;
        let vm: AgentVm = resp.into_result()
            .map_err(|e| anyhow::anyhow!("host-agent clone_vm: {e}"))?;
        info!(
            template = %template_id,
            clone = vm.name.as_str(),
            "VM clonée via host-agent"
        );
        Ok(agent_vm_to_provider(vm))
    }

    async fn start_vm(&self, provider_id: &str) -> Result<()> {
        let req = AgentRequest::StartVm {
            vm_id: provider_id.to_string(),
        };
        self.send_request(&req).await?
            .into_unit_result()
            .map_err(|e| anyhow::anyhow!("host-agent start_vm: {e}"))
    }

    async fn stop_vm(&self, provider_id: &str) -> Result<()> {
        let req = AgentRequest::StopVm {
            vm_id: provider_id.to_string(),
        };
        self.send_request(&req).await?
            .into_unit_result()
            .map_err(|e| anyhow::anyhow!("host-agent stop_vm: {e}"))
    }

    async fn delete_vm(&self, provider_id: &str) -> Result<()> {
        let req = AgentRequest::DeleteVm {
            vm_id: provider_id.to_string(),
        };
        self.send_request(&req).await?
            .into_unit_result()
            .map_err(|e| anyhow::anyhow!("host-agent delete_vm: {e}"))
    }

    async fn set_vsock_cid(&self, provider_id: &str, cid: u32) -> Result<()> {
        let req = AgentRequest::SetVsockCid {
            vm_id: provider_id.to_string(),
            cid,
        };
        self.send_request(&req).await?
            .into_unit_result()
            .map_err(|e| anyhow::anyhow!("host-agent set_vsock_cid: {e}"))
    }
}
