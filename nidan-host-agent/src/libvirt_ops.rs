//! Opérations libvirt pour le nidan-host-agent.
//!
//! Chaque fonction ouvre une connexion libvirt, exécute l'opération,
//! et retourne un `AgentVm` ou `()`. Les fonctions sont synchrones
//! (appelées depuis `spawn_blocking` dans le handler).

use anyhow::{bail, Result};
use nidan_proto::host_agent::AgentVm;
use tracing::{debug, info, warn};
use virt::connect::Connect;
use virt::domain::Domain;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;

// ── Connexion ───────────────────────────────────────────────────────────────

fn open(uri: &str) -> Result<Connect> {
    Connect::open(Some(uri))
        .map_err(|e| anyhow::anyhow!("connexion libvirt ({uri}): {e}"))
}

/// Vérifie la connectivité libvirt (appelé au démarrage).
pub fn check_connectivity(uri: &str) -> Result<()> {
    let conn = open(uri)?;
    let hv = conn.get_type()
        .map_err(|e| anyhow::anyhow!("get_type: {e}"))?;
    info!(hypervisor = %hv, "connectivité libvirt vérifiée");
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Résout un domaine par UUID ou nom, vérifie le préfixe.
fn lookup_and_verify(conn: &Connect, vm_id: &str, prefix: &str) -> Result<Domain> {
    let dom = Domain::lookup_by_uuid_string(conn, vm_id)
        .or_else(|_| Domain::lookup_by_name(conn, vm_id))
        .map_err(|e| anyhow::anyhow!("domaine {vm_id} introuvable: {e}"))?;

    // Vérification du préfixe (sécurité)
    let name = dom.get_name()
        .map_err(|e| anyhow::anyhow!("get_name: {e}"))?;
    if !name.starts_with(prefix) {
        bail!("domaine '{name}' hors périmètre (préfixe '{prefix}' requis)");
    }

    Ok(dom)
}

fn domain_to_agent_vm(dom: &Domain) -> Result<AgentVm> {
    let name = dom.get_name()
        .map_err(|e| anyhow::anyhow!("get_name: {e}"))?;
    let uuid = dom.get_uuid_string()
        .map_err(|e| anyhow::anyhow!("get_uuid: {e}"))?;
    let active = dom.is_active()
        .map_err(|e| anyhow::anyhow!("is_active: {e}"))?;

    Ok(AgentVm {
        provider_id: uuid,
        name,
        status: if active { "running".into() } else { "stopped".into() },
    })
}

// ── Opérations ──────────────────────────────────────────────────────────────

pub fn list_vms(uri: &str, prefix: &str) -> Result<Vec<AgentVm>> {
    let conn = open(uri)?;
    let domains = conn.list_all_domains(0)
        .map_err(|e| anyhow::anyhow!("list_all_domains: {e}"))?;

    let mut vms = Vec::new();
    for dom in &domains {
        if let Ok(name) = dom.get_name() {
            if name.starts_with(prefix) {
                match domain_to_agent_vm(dom) {
                    Ok(vm) => vms.push(vm),
                    Err(e) => warn!(domain = %name, error = %e, "skip"),
                }
            }
        }
    }
    Ok(vms)
}

pub fn get_status(uri: &str, vm_id: &str, prefix: &str) -> Result<AgentVm> {
    let conn = open(uri)?;
    let dom = lookup_and_verify(&conn, vm_id, prefix)?;
    domain_to_agent_vm(&dom)
}

pub fn clone_vm(
    uri: &str,
    template: &str,
    new_name: &str,
    storage_pool: &str,
    prefix: &str,
) -> Result<AgentVm> {
    let conn = open(uri)?;

    // Résoudre le template
    let tmpl = lookup_and_verify(&conn, template, prefix)?;
    if tmpl.is_active().unwrap_or(false) {
        bail!("template '{template}' actif — arrêter avant de cloner");
    }

    // Générer un nom si vide
    let name = if new_name.is_empty() {
        let suffix = &uuid::Uuid::new_v4().to_string()[..8];
        format!("{prefix}{suffix}")
    } else if !new_name.starts_with(prefix) {
        format!("{prefix}{new_name}")
    } else {
        new_name.to_string()
    };

    // Vérifier qu'aucun domaine n'a ce nom
    if Domain::lookup_by_name(&conn, &name).is_ok() {
        bail!("domaine '{name}' existe déjà");
    }

    let tmpl_xml = tmpl.get_xml_desc(0)
        .map_err(|e| anyhow::anyhow!("get_xml_desc: {e}"))?;

    // Cloner les volumes
    clone_volumes(&conn, &tmpl_xml, &name, storage_pool)?;

    // Réécrire le XML
    let new_xml = rewrite_domain_xml(&tmpl_xml, &name)?;

    // Définir le nouveau domaine
    let new_dom = Domain::define_xml(&conn, &new_xml)
        .map_err(|e| anyhow::anyhow!("define_xml: {e}"))?;

    info!(template = %template, clone = %name, "VM clonée");
    domain_to_agent_vm(&new_dom)
}

pub fn start_vm(uri: &str, vm_id: &str, prefix: &str) -> Result<()> {
    let conn = open(uri)?;
    let dom = lookup_and_verify(&conn, vm_id, prefix)?;
    dom.create()
        .map_err(|e| anyhow::anyhow!("start {vm_id}: {e}"))?;
    Ok(())
}

pub fn stop_vm(uri: &str, vm_id: &str, prefix: &str) -> Result<()> {
    let conn = open(uri)?;
    let dom = lookup_and_verify(&conn, vm_id, prefix)?;
    match dom.shutdown() {
        Ok(_) => info!(vm_id = %vm_id, "shutdown initié"),
        Err(e) => {
            warn!(vm_id = %vm_id, error = %e, "shutdown échoué → destroy");
            dom.destroy()
                .map_err(|e2| anyhow::anyhow!("destroy {vm_id}: {e2}"))?;
        }
    }
    Ok(())
}

pub fn delete_vm(uri: &str, vm_id: &str, prefix: &str) -> Result<()> {
    let conn = open(uri)?;
    let dom = lookup_and_verify(&conn, vm_id, prefix)?;

    // Arrêter si active
    if dom.is_active().unwrap_or(false) {
        let _ = dom.destroy();
    }

    // Supprimer les volumes
    let xml = dom.get_xml_desc(0)
        .map_err(|e| anyhow::anyhow!("get_xml_desc: {e}"))?;
    for path in extract_disk_paths(&xml) {
        match StorageVol::lookup_by_path(&conn, &path) {
            Ok(vol) => {
                if let Err(e) = vol.delete(0) {
                    warn!(path = %path, error = %e, "suppression volume échouée");
                } else {
                    debug!(path = %path, "volume supprimé");
                }
            }
            Err(_) => debug!(path = %path, "volume introuvable — skip"),
        }
    }

    dom.undefine()
        .map_err(|e| anyhow::anyhow!("undefine {vm_id}: {e}"))?;
    Ok(())
}

pub fn set_vsock_cid(uri: &str, vm_id: &str, cid: u32, prefix: &str) -> Result<()> {
    let conn = open(uri)?;
    let dom = lookup_and_verify(&conn, vm_id, prefix)?;

    let xml = dom.get_xml_desc(0)
        .map_err(|e| anyhow::anyhow!("get_xml_desc: {e}"))?;
    let new_xml = set_vsock_in_xml(&xml, cid)?;

    Domain::define_xml(&conn, &new_xml)
        .map_err(|e| anyhow::anyhow!("redefine vsock: {e}"))?;
    Ok(())
}

// ── Helpers XML ─────────────────────────────────────────────────────────────
// NOTE : ces fonctions sont dupliquées depuis nidan-broker/src/provider/libvirt.rs.
// À terme, elles seront extraites dans un module partagé (nidan-common ou dédié).

fn rewrite_domain_xml(xml: &str, new_name: &str) -> Result<String> {
    let mut out = xml.to_string();

    // Remplacer <name>
    if let (Some(s), Some(e)) = (out.find("<name>"), out.find("</name>")) {
        out.replace_range(s + "<name>".len()..e, new_name);
    } else {
        bail!("XML: <name> introuvable");
    }

    // Supprimer <uuid>
    if let Some(s) = out.find("<uuid>") {
        if let Some(e) = out[s..].find("</uuid>") {
            let end = s + e + "</uuid>".len();
            let trim = if out.as_bytes().get(end) == Some(&b'\n') { end + 1 } else { end };
            out.replace_range(s..trim, "");
        }
    }

    // Réécrire les chemins disque
    out = rewrite_disk_paths(&out, new_name);
    Ok(out)
}

fn clone_volumes(conn: &Connect, xml: &str, new_name: &str, pool_name: &str) -> Result<()> {
    let pool = StoragePool::lookup_by_name(conn, pool_name)
        .map_err(|e| anyhow::anyhow!("pool '{pool_name}' introuvable: {e}"))?;

    for src_path in extract_disk_paths(xml) {
        let (dir, _base, ext) = split_path(&src_path);
        let new_path = format!("{dir}/{new_name}{ext}");

        match StorageVol::lookup_by_path(conn, &src_path) {
            Ok(_src_vol) => {
                // Thin clone : backing file au lieu de copie complète.
                // Instantané (~0s) au lieu de ~50s pour 16 Go sur HDD.
                let output = std::process::Command::new("qemu-img")
                    .args(["create", "-f", "qcow2", "-b", &src_path, "-F", "qcow2", &new_path])
                    .output()
                    .map_err(|e| anyhow::anyhow!("qemu-img create: {e}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("thin clone {src_path} → {new_path}: {stderr}");
                }
                // Rafraîchir le pool pour que libvirt voie le nouveau volume
                let _ = pool.refresh(0);
                debug!(src = %src_path, dst = %new_path, "volume thin-cloné (backing file)");
            }
            Err(e) => warn!(path = %src_path, error = %e, "volume source introuvable"),
        }
    }
    Ok(())
}

fn set_vsock_in_xml(xml: &str, cid: u32) -> Result<String> {
    let elem = format!(
        "    <vsock model='virtio'>\n      <cid auto='no' address='{cid}'/>\n    </vsock>"
    );
    let mut out = xml.to_string();

    if let Some(s) = out.find("<vsock") {
        if let Some(re) = out[s..].find("</vsock>") {
            out.replace_range(s..s + re + "</vsock>".len(), &elem);
            return Ok(out);
        }
    }
    if let Some(pos) = out.find("</devices>") {
        out.insert_str(pos, &format!("{elem}\n  "));
        return Ok(out);
    }
    bail!("XML: </devices> introuvable")
}

fn extract_disk_paths(xml: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for quote in ["'", "\""] {
        let pat = format!("<source file={quote}");
        let mut pos = 0;
        while let Some(idx) = xml[pos..].find(&pat) {
            let start = pos + idx + pat.len();
            if let Some(end) = xml[start..].find(quote) {
                paths.push(xml[start..start + end].to_string());
                pos = start + end;
            } else { break; }
        }
    }
    paths
}

fn split_path(path: &str) -> (String, String, String) {
    let slash = path.rfind('/').unwrap_or(0);
    let dir = &path[..slash];
    let file = &path[slash + 1..];
    if let Some(dot) = file.rfind('.') {
        (dir.into(), file[..dot].into(), file[dot..].into())
    } else {
        (dir.into(), file.into(), String::new())
    }
}

fn rewrite_disk_paths(xml: &str, new_name: &str) -> String {
    let mut out = xml.to_string();
    for quote in ["'", "\""] {
        let pat = format!("<source file={quote}");
        let mut search = 0;
        while let Some(idx) = out[search..].find(&pat) {
            let abs = search + idx + pat.len();
            if let Some(end) = out[abs..].find(quote) {
                let old_path = out[abs..abs + end].to_string();
                let (dir, _base, ext) = split_path(&old_path);
                let new_path = format!("{dir}/{new_name}{ext}");
                out.replace_range(abs..abs + end, &new_path);
                search = abs + new_path.len();
            } else { break; }
        }
    }
    out
}
