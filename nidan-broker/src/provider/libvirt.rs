//! Backend libvirt pour le trait [`VmProvider`].
//!
//! Implémente la gestion de VMs NIDAN via l'API libvirt (KVM/QEMU).
//! Compilé uniquement avec `--features provider-libvirt`.
//!
//! ## Architecture
//!
//! - Chaque opération ouvre une connexion libvirt dédiée et l'exécute dans
//!   un `spawn_blocking` (l'API libvirt est synchrone et bloquante).
//! - Les VMs NIDAN sont identifiées par leur UUID libvirt comme `provider_id`.
//! - Seules les VMs dont le nom commence par `vm_prefix` (défaut: `nidan-`)
//!   sont visibles via `list_vms`.
//!
//! ## Prérequis
//!
//! - `libvirt-dev` / `libvirt-devel` installé (pour la compilation)
//! - `libvirtd` actif sur la machine cible
//! - L'utilisateur du broker doit être dans le groupe `libvirt`
//!   (ou l'URI doit utiliser `qemu+ssh://`)

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tracing::{debug, info, warn};
use virt::connect::Connect;
use virt::domain::Domain;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;

use super::{ProviderVm, ProviderVmStatus, VmProvider};
use crate::config::LibvirtProviderConfig;

// ── Constantes d'état libvirt ───────────────────────────────────────────────
// cf. libvirt.h : virDomainState
#[allow(dead_code)]
const VIR_DOMAIN_NOSTATE:     u32 = 0;
const VIR_DOMAIN_RUNNING:     u32 = 1;
#[allow(dead_code)]
const VIR_DOMAIN_BLOCKED:     u32 = 2;
#[allow(dead_code)]
const VIR_DOMAIN_PAUSED:      u32 = 3;
#[allow(dead_code)]
const VIR_DOMAIN_SHUTDOWN:    u32 = 4;
const VIR_DOMAIN_SHUTOFF:     u32 = 5;
#[allow(dead_code)]
const VIR_DOMAIN_CRASHED:     u32 = 6;
#[allow(dead_code)]
const VIR_DOMAIN_PMSUSPENDED: u32 = 7;

// ── Provider ────────────────────────────────────────────────────────────────

/// Provider libvirt pour KVM/QEMU.
pub struct LibvirtProvider {
    config: LibvirtProviderConfig,
}

impl LibvirtProvider {
    /// Construit le provider et vérifie la connectivité.
    pub fn new(config: &LibvirtProviderConfig) -> Result<Self> {
        // Test de connexion au démarrage
        let conn = open_conn(&config.uri)?;
        let hv_type = conn
            .get_type()
            .map_err(|e| anyhow::anyhow!("get_type: {e}"))?;
        let hv_ver = conn
            .get_hyp_version()
            .map_err(|e| anyhow::anyhow!("get_hyp_version: {e}"))?;
        info!(
            uri       = %config.uri,
            hypervisor = %hv_type,
            version   = hv_ver,
            "provider libvirt initialisé"
        );
        Ok(Self {
            config: config.clone(),
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Ouvre une connexion libvirt.
fn open_conn(uri: &str) -> Result<Connect> {
    Connect::open(Some(uri)).map_err(|e| anyhow::anyhow!("connexion libvirt ({uri}): {e}"))
}

/// Convertit un domaine libvirt en [`ProviderVm`].
fn domain_to_vm(dom: &Domain) -> Result<ProviderVm> {
    let name = dom
        .get_name()
        .map_err(|e| anyhow::anyhow!("get_name: {e}"))?;
    let uuid = dom
        .get_uuid_string()
        .map_err(|e| anyhow::anyhow!("get_uuid: {e}"))?;
    let info = dom
        .get_info()
        .map_err(|e| anyhow::anyhow!("get_info: {e}"))?;

    let status = match info.state as u32 {
        VIR_DOMAIN_RUNNING | VIR_DOMAIN_BLOCKED => ProviderVmStatus::Running,
        VIR_DOMAIN_SHUTOFF => ProviderVmStatus::Stopped,
        VIR_DOMAIN_PAUSED => ProviderVmStatus::Unknown("paused".into()),
        VIR_DOMAIN_SHUTDOWN => ProviderVmStatus::Unknown("shutting-down".into()),
        VIR_DOMAIN_CRASHED => ProviderVmStatus::Unknown("crashed".into()),
        VIR_DOMAIN_PMSUSPENDED => ProviderVmStatus::Unknown("pm-suspended".into()),
        other => ProviderVmStatus::Unknown(format!("state-{other}")),
    };

    Ok(ProviderVm {
        provider_id: uuid,
        name: Some(name),
        status,
    })
}

/// Cherche un domaine par UUID ou par nom.
fn lookup_domain(conn: &Connect, id: &str) -> Result<Domain> {
    Domain::lookup_by_uuid_string(conn, id)
        .or_else(|_| Domain::lookup_by_name(conn, id))
        .map_err(|e| anyhow::anyhow!("domaine {id} introuvable: {e}"))
}

// ── Impl VmProvider ─────────────────────────────────────────────────────────

#[async_trait]
impl VmProvider for LibvirtProvider {
    fn backend_name(&self) -> &'static str {
        "libvirt"
    }

    async fn list_vms(&self) -> Result<Vec<ProviderVm>> {
        let uri = self.config.uri.clone();
        let prefix = self.config.vm_prefix.clone();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&uri)?;
            // flags = 0 → tous les domaines (actifs + inactifs)
            let domains = conn
                .list_all_domains(0)
                .map_err(|e| anyhow::anyhow!("list_all_domains: {e}"))?;

            let mut vms = Vec::new();
            for dom in &domains {
                if let Ok(name) = dom.get_name() {
                    if name.starts_with(&prefix) {
                        match domain_to_vm(dom) {
                            Ok(vm) => vms.push(vm),
                            Err(e) => warn!(domain = %name, error = %e, "skip"),
                        }
                    }
                }
            }
            debug!(count = vms.len(), "VMs listées");
            Ok(vms)
        })
        .await
        .context("spawn_blocking list_vms")?
    }

    async fn get_status(&self, provider_id: &str) -> Result<ProviderVm> {
        let uri = self.config.uri.clone();
        let id = provider_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&uri)?;
            let dom = lookup_domain(&conn, &id)?;
            domain_to_vm(&dom)
        })
        .await
        .context("spawn_blocking get_status")?
    }

    async fn clone_vm(
        &self,
        template_id: &str,
        new_name: &str,
    ) -> Result<ProviderVm> {
        let uri = self.config.uri.clone();
        let template = template_id.to_string();
        let prefix = self.config.vm_prefix.clone();
        let pool_name = self.config.storage_pool.clone();

        // Générer un nom unique si new_name est vide
        let name = if new_name.is_empty() {
            let suffix = uuid::Uuid::new_v4().to_string()[..8].to_string();
            format!("{prefix}{suffix}")
        } else if !new_name.starts_with(&prefix) {
            // Forcer le préfixe NIDAN pour le cloisonnement
            format!("{prefix}{new_name}")
        } else {
            new_name.to_string()
        };

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&uri)?;

            // 1. Résoudre et valider le template
            let tmpl = lookup_domain(&conn, &template)?;

            // Vérifier que le template est arrêté (on ne clone pas une VM active)
            if tmpl.is_active().unwrap_or(false) {
                bail!("template {template} est actif — arrêter avant de cloner");
            }

            let tmpl_xml = tmpl
                .get_xml_desc(0)
                .map_err(|e| anyhow::anyhow!("get_xml_desc template: {e}"))?;

            // Vérifier qu'un domaine portant ce nom n'existe pas déjà
            if Domain::lookup_by_name(&conn, &name).is_ok() {
                bail!("un domaine nommé '{name}' existe déjà");
            }

            // 2. Cloner les volumes disque
            clone_volumes(&conn, &tmpl_xml, &name, &pool_name)?;

            // 3. Modifier le XML pour le nouveau domaine
            let new_xml = rewrite_domain_xml(&tmpl_xml, &name)?;

            // 4. Définir le nouveau domaine
            let new_dom = Domain::define_xml(&conn, &new_xml)
                .map_err(|e| anyhow::anyhow!("domain_define_xml: {e}"))?;

            info!(
                template = %template,
                new_name = %name,
                "VM clonée"
            );
            domain_to_vm(&new_dom)
        })
        .await
        .context("spawn_blocking clone_vm")?
    }

    async fn start_vm(&self, provider_id: &str) -> Result<()> {
        let uri = self.config.uri.clone();
        let id = provider_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&uri)?;
            let dom = lookup_domain(&conn, &id)?;
            dom.create()
                .map_err(|e| anyhow::anyhow!("start {id}: {e}"))?;
            info!(uuid = %id, "VM démarrée");
            Ok(())
        })
        .await
        .context("spawn_blocking start_vm")?
    }

    async fn stop_vm(&self, provider_id: &str) -> Result<()> {
        let uri = self.config.uri.clone();
        let id = provider_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&uri)?;
            let dom = lookup_domain(&conn, &id)?;
            // Shutdown propre en premier ; destroy si échec
            match dom.shutdown() {
                Ok(_) => info!(uuid = %id, "shutdown initié"),
                Err(e) => {
                    warn!(uuid = %id, error = %e, "shutdown échoué → destroy");
                    dom.destroy()
                        .map_err(|e2| anyhow::anyhow!("destroy {id}: {e2}"))?;
                    info!(uuid = %id, "VM forcée à l'arrêt");
                }
            }
            Ok(())
        })
        .await
        .context("spawn_blocking stop_vm")?
    }

    async fn delete_vm(&self, provider_id: &str) -> Result<()> {
        let uri = self.config.uri.clone();
        let id = provider_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&uri)?;
            let dom = lookup_domain(&conn, &id)?;

            // Arrêter si active
            if dom.is_active().unwrap_or(false) {
                let _ = dom.destroy();
            }

            // Supprimer les volumes disque associés avant undefine
            let xml = dom
                .get_xml_desc(0)
                .map_err(|e| anyhow::anyhow!("get_xml_desc pour suppression: {e}"))?;
            for disk_path in extract_disk_paths(&xml) {
                match StorageVol::lookup_by_path(&conn, &disk_path) {
                    Ok(vol) => {
                        if let Err(e) = vol.delete(0) {
                            warn!(
                                path = %disk_path,
                                error = %e,
                                "échec suppression volume — continue"
                            );
                        } else {
                            debug!(path = %disk_path, "volume supprimé");
                        }
                    }
                    Err(e) => {
                        debug!(
                            path = %disk_path,
                            error = %e,
                            "volume introuvable — skip"
                        );
                    }
                }
            }

            dom.undefine()
                .map_err(|e| anyhow::anyhow!("undefine {id}: {e}"))?;
            info!(uuid = %id, "VM et volumes supprimés");
            Ok(())
        })
        .await
        .context("spawn_blocking delete_vm")?
    }

    async fn set_vsock_cid(&self, provider_id: &str, cid: u32) -> Result<()> {
        let uri = self.config.uri.clone();
        let id = provider_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&uri)?;
            let dom = lookup_domain(&conn, &id)?;

            let xml = dom
                .get_xml_desc(0)
                .map_err(|e| anyhow::anyhow!("get_xml_desc: {e}"))?;
            let new_xml = set_vsock_in_xml(&xml, cid)?;

            Domain::define_xml(&conn, &new_xml)
                .map_err(|e| anyhow::anyhow!("redefine vsock: {e}"))?;
            info!(uuid = %id, cid = cid, "vsock CID configuré");
            Ok(())
        })
        .await
        .context("spawn_blocking set_vsock_cid")?
    }
}

// ── Helpers XML ─────────────────────────────────────────────────────────────

/// Réécrit le XML d'un template pour en faire un clone :
/// - nouveau `<name>`
/// - `<uuid>` supprimé (libvirt en génère un)
/// - chemins de disques mis à jour
fn rewrite_domain_xml(xml: &str, new_name: &str) -> Result<String> {
    let mut out = xml.to_string();

    // 1. Remplacer <name>old</name>
    let (ns, ne) = find_tag_content(&out, "name")
        .context("XML: balise <name> introuvable")?;
    out.replace_range(ns..ne, new_name);

    // 2. Supprimer <uuid>...</uuid>
    if let Some((start, end)) = find_full_tag(&out, "uuid") {
        // Supprimer aussi le \n suivant si présent
        let trim = if out.as_bytes().get(end) == Some(&b'\n') {
            end + 1
        } else {
            end
        };
        out.replace_range(start..trim, "");
    }

    // 3. Mettre à jour les chemins de source file=
    //    <source file='/var/lib/libvirt/images/template.qcow2'/>
    //    → remplacer le basename par new_name + même extension
    out = rewrite_disk_paths(&out, new_name);

    Ok(out)
}

/// Clone les volumes disque référencés dans le XML du template.
fn clone_volumes(
    conn: &Connect,
    tmpl_xml: &str,
    new_name: &str,
    pool_name: &str,
) -> Result<()> {
    let pool = StoragePool::lookup_by_name(conn, pool_name)
        .map_err(|e| anyhow::anyhow!("pool '{pool_name}' introuvable: {e}"))?;

    for src_path in extract_disk_paths(tmpl_xml) {
        let (dir, _old_base, ext) = split_path(&src_path);
        let new_path = format!("{dir}/{new_name}{ext}");

        match StorageVol::lookup_by_path(conn, &src_path) {
            Ok(src_vol) => {
                let vol_xml = format!(
                    "<volume>\n  <name>{new_name}{ext}</name>\n  \
                     <target><path>{new_path}</path></target>\n</volume>"
                );
                StorageVol::create_xml_from(&pool, &vol_xml, &src_vol, 0)
                    .map_err(|e| {
                        anyhow::anyhow!("clone volume {src_path} → {new_path}: {e}")
                    })?;
                debug!(src = %src_path, dst = %new_path, "volume cloné");
            }
            Err(e) => {
                warn!(
                    path = %src_path,
                    error = %e,
                    "volume source introuvable — skip"
                );
            }
        }
    }
    Ok(())
}

/// Ajoute ou met à jour le device vsock dans le XML du domaine.
fn set_vsock_in_xml(xml: &str, cid: u32) -> Result<String> {
    let vsock_elem = format!(
        "    <vsock model='virtio'>\n\
         \x20     <cid auto='no' address='{cid}'/>\n\
         \x20   </vsock>"
    );

    let mut out = xml.to_string();

    // Remplacer un vsock existant
    if let Some(start) = out.find("<vsock") {
        if let Some(rel_end) = out[start..].find("</vsock>") {
            let end = start + rel_end + "</vsock>".len();
            out.replace_range(start..end, &vsock_elem);
            return Ok(out);
        }
    }

    // Sinon insérer avant </devices>
    if let Some(pos) = out.find("</devices>") {
        out.insert_str(pos, &format!("{vsock_elem}\n  "));
        return Ok(out);
    }

    bail!("XML invalide: </devices> introuvable")
}

// ── Micro-parseur XML utilitaire ────────────────────────────────────────────

/// Retourne (content_start, content_end) du PREMIER <tag>content</tag>.
fn find_tag_content(xml: &str, tag: &str) -> Option<(usize, usize)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some((start, end))
}

/// Retourne (tag_start, tag_end) incluant les balises ouvrante et fermante.
fn find_full_tag(xml: &str, tag: &str) -> Option<(usize, usize)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)?;
    let end = xml[start..].find(&close)? + start + close.len();
    Some((start, end))
}

/// Extrait les chemins de fichiers disque depuis le XML.
fn extract_disk_paths(xml: &str) -> Vec<String> {
    let pattern = "<source file='";
    let mut paths = Vec::new();
    let mut pos = 0;
    while let Some(idx) = xml[pos..].find(pattern) {
        let start = pos + idx + pattern.len();
        if let Some(end) = xml[start..].find("'") {
            paths.push(xml[start..start + end].to_string());
            pos = start + end;
        } else {
            break;
        }
    }
    // Aussi chercher avec guillemets doubles
    let pattern2 = "<source file=\"";
    pos = 0;
    while let Some(idx) = xml[pos..].find(pattern2) {
        let start = pos + idx + pattern2.len();
        if let Some(end) = xml[start..].find("\"") {
            paths.push(xml[start..start + end].to_string());
            pos = start + end;
        } else {
            break;
        }
    }
    paths
}

/// Sépare un chemin en (répertoire, basename_sans_ext, extension_avec_dot).
fn split_path(path: &str) -> (String, String, String) {
    let last_slash = path.rfind('/').unwrap_or(0);
    let dir = &path[..last_slash];
    let filename = &path[last_slash + 1..];
    if let Some(dot) = filename.rfind('.') {
        (
            dir.to_string(),
            filename[..dot].to_string(),
            filename[dot..].to_string(),
        )
    } else {
        (dir.to_string(), filename.to_string(), String::new())
    }
}

/// Remplace les basenames des disques dans le XML par new_name.
fn rewrite_disk_paths(xml: &str, new_name: &str) -> String {
    let mut out = xml.to_string();
    for quote in ["'", "\""] {
        let pattern = format!("<source file={quote}");
        let mut search = 0;
        while let Some(idx) = out[search..].find(&pattern) {
            let abs = search + idx + pattern.len();
            if let Some(end) = out[abs..].find(quote) {
                let old_path = out[abs..abs + end].to_string();
                let (dir, _base, ext) = split_path(&old_path);
                let new_path = format!("{dir}/{new_name}{ext}");
                out.replace_range(abs..abs + end, &new_path);
                search = abs + new_path.len();
            } else {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrite_domain_xml() {
        let xml = r#"<domain type='kvm'>
  <name>nidan-template</name>
  <uuid>12345678-1234-1234-1234-123456789abc</uuid>
  <devices>
    <disk type='file' device='disk'>
      <source file='/var/lib/libvirt/images/nidan-template.qcow2'/>
    </disk>
  </devices>
</domain>"#;
        let result = rewrite_domain_xml(xml, "nidan-clone-001").unwrap();
        assert!(result.contains("<name>nidan-clone-001</name>"));
        assert!(!result.contains("<uuid>"));
        assert!(result.contains("nidan-clone-001.qcow2"));
        assert!(!result.contains("nidan-template.qcow2"));
    }

    #[test]
    fn test_set_vsock_new() {
        let xml = "<domain><devices><disk/></devices></domain>";
        let result = set_vsock_in_xml(xml, 42).unwrap();
        assert!(result.contains("address='42'"));
        assert!(result.contains("<vsock"));
    }

    #[test]
    fn test_set_vsock_replace() {
        let xml = "<domain><devices><vsock model='virtio'><cid auto='no' address='10'/></vsock></devices></domain>";
        let result = set_vsock_in_xml(xml, 42).unwrap();
        assert!(result.contains("address='42'"));
        assert!(!result.contains("address='10'"));
    }

    #[test]
    fn test_rewrite_preserves_devices() {
        // Vérifie que rewrite_domain_xml ne supprime pas les autres devices
        let xml = r#"<domain type='kvm'>
  <name>tmpl</name>
  <uuid>aaa</uuid>
  <devices>
    <disk type='file' device='disk'>
      <source file='/pool/tmpl.qcow2'/>
    </disk>
    <interface type='network'>
      <source network='default'/>
    </interface>
    <vsock model='virtio'>
      <cid auto='no' address='5'/>
    </vsock>
  </devices>
</domain>"#;
        let result = rewrite_domain_xml(xml, "clone-1").unwrap();
        assert!(result.contains("<name>clone-1</name>"));
        assert!(result.contains("<interface"), "interface préservée");
        assert!(result.contains("<vsock"), "vsock préservé");
        assert!(result.contains("clone-1.qcow2"));
    }

    #[test]
    fn test_extract_disk_paths() {
        let xml = "<disk><source file='/a/b.qcow2'/></disk><disk><source file=\"/c/d.raw\"/></disk>";
        let paths = extract_disk_paths(xml);
        assert_eq!(paths, vec!["/a/b.qcow2", "/c/d.raw"]);
    }
}
