//! Dispatch des requêtes agent vers les opérations libvirt.

use nidan_proto::host_agent::{AgentRequest, AgentResponse};
use tracing::info;

use crate::config::HostAgentConfig;
use crate::libvirt_ops;

/// Traite une requête et retourne la réponse.
///
/// Les opérations libvirt sont bloquantes — elles sont exécutées dans un
/// `spawn_blocking` pour ne pas bloquer le runtime tokio.
pub async fn handle_request(
    req: AgentRequest,
    cfg: &HostAgentConfig,
) -> AgentResponse {
    let uri = cfg.libvirt.uri.clone();
    let pool_name = cfg.libvirt.storage_pool.clone();
    let prefix = cfg.libvirt.vm_prefix.clone();

    // Validation du préfixe (sécurité : périmètre NIDAN uniquement)
    if let Err(resp) = validate_prefix(&req, &prefix) {
        return resp;
    }

    tokio::task::spawn_blocking(move || {
        dispatch(req, &uri, &pool_name, &prefix)
    })
    .await
    .unwrap_or_else(|e| AgentResponse::err(format!("task panic: {e}")))
}

/// Vérifie que les opérations portent sur des VMs du périmètre NIDAN.
fn validate_prefix(req: &AgentRequest, prefix: &str) -> Result<(), AgentResponse> {
    match req {
        AgentRequest::CloneVm { template, .. } => {
            if !template.starts_with(prefix) {
                return Err(AgentResponse::err(format!(
                    "template '{template}' hors périmètre (préfixe '{prefix}' requis)"
                )));
            }
        }
        AgentRequest::ListVms { prefix: req_prefix } => {
            if !req_prefix.starts_with(prefix) {
                return Err(AgentResponse::err(format!(
                    "préfixe '{req_prefix}' hors périmètre (doit commencer par '{prefix}')"
                )));
            }
        }
        // Pour les opérations par UUID/nom, la validation est faite
        // dans libvirt_ops après résolution du domaine.
        _ => {}
    }
    Ok(())
}

/// Dispatch synchrone des opérations (exécuté dans spawn_blocking).
fn dispatch(
    req: AgentRequest,
    uri: &str,
    storage_pool: &str,
    vm_prefix: &str,
) -> AgentResponse {
    match req {
        AgentRequest::ListVms { prefix } => {
            match libvirt_ops::list_vms(uri, &prefix) {
                Ok(vms) => {
                    info!(count = vms.len(), "list_vms OK");
                    AgentResponse::ok_with(&vms)
                        .unwrap_or_else(|e| AgentResponse::err(e.to_string()))
                }
                Err(e) => AgentResponse::err(e.to_string()),
            }
        }

        AgentRequest::GetStatus { vm_id } => {
            match libvirt_ops::get_status(uri, &vm_id, vm_prefix) {
                Ok(vm) => {
                    info!(vm = %vm.name, status = %vm.status, "get_status OK");
                    AgentResponse::ok_with(&vm)
                        .unwrap_or_else(|e| AgentResponse::err(e.to_string()))
                }
                Err(e) => AgentResponse::err(e.to_string()),
            }
        }

        AgentRequest::CloneVm { template, new_name } => {
            match libvirt_ops::clone_vm(uri, &template, &new_name, storage_pool, vm_prefix) {
                Ok(vm) => {
                    info!(name = %vm.name, "clone_vm OK");
                    AgentResponse::ok_with(&vm)
                        .unwrap_or_else(|e| AgentResponse::err(e.to_string()))
                }
                Err(e) => AgentResponse::err(e.to_string()),
            }
        }

        AgentRequest::StartVm { vm_id } => {
            match libvirt_ops::start_vm(uri, &vm_id, vm_prefix) {
                Ok(()) => {
                    info!(vm_id = %vm_id, "start_vm OK");
                    AgentResponse::ok_empty()
                }
                Err(e) => AgentResponse::err(e.to_string()),
            }
        }

        AgentRequest::StopVm { vm_id } => {
            match libvirt_ops::stop_vm(uri, &vm_id, vm_prefix) {
                Ok(()) => {
                    info!(vm_id = %vm_id, "stop_vm OK");
                    AgentResponse::ok_empty()
                }
                Err(e) => AgentResponse::err(e.to_string()),
            }
        }

        AgentRequest::DeleteVm { vm_id } => {
            match libvirt_ops::delete_vm(uri, &vm_id, vm_prefix) {
                Ok(()) => {
                    info!(vm_id = %vm_id, "delete_vm OK");
                    AgentResponse::ok_empty()
                }
                Err(e) => AgentResponse::err(e.to_string()),
            }
        }

        AgentRequest::SetVsockCid { vm_id, cid } => {
            match libvirt_ops::set_vsock_cid(uri, &vm_id, cid, vm_prefix) {
                Ok(()) => {
                    info!(vm_id = %vm_id, cid = cid, "set_vsock_cid OK");
                    AgentResponse::ok_empty()
                }
                Err(e) => AgentResponse::err(e.to_string()),
            }
        }
    }
}
