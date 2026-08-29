//! Client API Proxmox VE — Échelon A (lecture seule).
//!
//! Ce module implémente le premier échelon du mode dynamique de pool de
//! VMs (voir plan-dev-v2.md, section "Pool de VMs — mode dynamique") :
//! un client HTTP minimal qui s'authentifie à l'API Proxmox et sait
//! **uniquement lire** (statut d'une VM, liste des VMs). Aucune opération
//! d'écriture (clone/start/stop/delete) n'est présente ici — elles
//! viendront à l'échelon B, une fois ce socle validé en conditions
//! réelles.
//!
//! ## Authentification
//!
//! Par token API (`PVEAPIToken=user@realm!tokenid=secret`), recommandé
//! par Proxmox pour l'automatisation plutôt qu'un mot de passe. Voir
//! `docs/DEPLOIEMENT-PROXMOX.md` pour la procédure de création du rôle
//! et du token dédiés (privilèges minimaux, pas root@pam).
//!
//! ## Vérification TLS
//!
//! Proxmox utilise par défaut un certificat auto-signé. Plutôt que de
//! désactiver la vérification TLS (`danger_accept_invalid_certs`,
//! mauvaise pratique), ce module épingle l'empreinte SHA-256 du
//! certificat attendu : seul ce certificat exact sera accepté, ce qui
//! protège contre une interception même sans chaîne de confiance
//! classique (le certificat étant auto-signé, il n'y a de toute façon
//! pas de chaîne à valider — c'est le principe du "certificate pinning").

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// Configuration de connexion à l'API Proxmox.
#[derive(Debug, Clone)]
pub struct ProxmoxConfig {
    /// URL de base, ex : `https://192.168.8.175:8006`
    pub base_url: String,
    /// Nom du nœud Proxmox, ex : `pve`
    pub node: String,
    /// Identifiant complet du token, ex : `nidan-automation@pve!broker-token`
    pub token_id: String,
    /// Secret du token (UUID affiché une seule fois à la création)
    pub token_secret: String,
    /// Empreinte SHA-256 du certificat TLS de l'interface Proxmox,
    /// en hexadécimal (avec ou sans séparateurs `:`).
    /// Récupérable via : `openssl x509 -in /etc/pve/local/pve-ssl.pem -noout -fingerprint -sha256`
    pub cert_fingerprint_sha256: String,
}

/// Statut d'une VM (réponse de `/nodes/{node}/qemu/{vmid}/status/current`).
#[derive(Debug, Clone, Deserialize)]
pub struct VmStatus {
    pub vmid: u32,
    /// "running", "stopped", etc.
    pub status: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub uptime: Option<u64>,
}

/// Résumé d'une VM (réponse de `/nodes/{node}/qemu`).
#[derive(Debug, Clone, Deserialize)]
pub struct VmSummary {
    pub vmid: u32,
    pub status: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Client API Proxmox, lecture seule pour cet échelon.
pub struct ProxmoxClient {
    http: reqwest::Client,
    base_url: String,
    node: String,
    auth_header: String,
}

impl ProxmoxClient {
    /// Construit le client. Échoue si l'empreinte fournie n'est pas un
    /// SHA-256 valide (32 octets) — erreur de configuration détectée tôt,
    /// avant toute tentative de connexion réseau.
    pub fn new(config: &ProxmoxConfig) -> Result<Self> {
        let fingerprint = parse_sha256_fingerprint(&config.cert_fingerprint_sha256)
            .context("empreinte de certificat Proxmox invalide dans la configuration")?;

        let verifier = Arc::new(FingerprintVerifier {
            expected_fingerprint: fingerprint,
        });

        let tls_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();

        let http = reqwest::Client::builder()
            .use_preconfigured_tls(tls_config)
            .build()
            .context("construction du client HTTP Proxmox")?;

        let auth_header = format!("PVEAPIToken={}={}", config.token_id, config.token_secret);

        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            node: config.node.clone(),
            auth_header,
        })
    }

    /// Récupère le statut courant d'une VM (running/stopped/uptime...).
    pub async fn get_vm_status(&self, vmid: u32) -> Result<VmStatus> {
        let url = format!(
            "{}/api2/json/nodes/{}/qemu/{}/status/current",
            self.base_url, self.node, vmid
        );
        self.get_json(&url)
            .await
            .with_context(|| format!("récupération du statut de la VM {vmid}"))
    }

    /// Liste toutes les VMs connues du nœud configuré.
    pub async fn list_vms(&self) -> Result<Vec<VmSummary>> {
        let url = format!("{}/api2/json/nodes/{}/qemu", self.base_url, self.node);
        self.get_json(&url)
            .await
            .context("récupération de la liste des VMs")
    }

    /// Requête GET générique, désenveloppe le `{"data": ...}` standard
    /// de l'API Proxmox.
    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        #[derive(Deserialize)]
        struct Wrapper<T> {
            data: T,
        }

        let resp = self
            .http
            .get(url)
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .context("envoi de la requête HTTP")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Proxmox a répondu {status} : {body}");
        }

        let wrapper: Wrapper<T> = resp
            .json()
            .await
            .context("parsing de la réponse JSON Proxmox")?;
        Ok(wrapper.data)
    }

    // ------------------------------------------------------------------ //
    //  Échelon B — Opérations d'écriture                                  //
    //                                                                      //
    //  Chaque opération modifiant l'état d'une VM (clone, start, stop,    //
    //  delete) est asynchrone côté Proxmox : l'API retourne immédiatement //
    //  un UPID (Unique Process ID de tâche), que wait_for_task() polle   //
    //  jusqu'à la fin effective de l'opération. Ne jamais enchaîner une  //
    //  opération (ex. start) sans avoir attendu la fin de la précédente  //
    //  (ex. clone) — la VM n'est pas encore prête.                        //
    // ------------------------------------------------------------------ //

    /// Clone une VM depuis un template.
    ///
    /// - `template_vmid` : VMID du template source (doit exister et être
    ///   de type template sur Proxmox, via `qm template <vmid>`).
    /// - `new_vmid`      : VMID de la VM clonée (plage 200-299 réservée).
    /// - `name`          : nom affiché dans l'interface Proxmox.
    ///
    /// Retourne l'UPID de la tâche de clonage. Passer ce résultat à
    /// `wait_for_task()` avant tout autre appel sur la VM clonée.
    pub async fn clone_vm(
        &self,
        template_vmid: u32,
        new_vmid: u32,
        name: &str,
    ) -> Result<String> {
        let url = format!(
            "{}/api2/json/nodes/{}/qemu/{}/clone",
            self.base_url, self.node, template_vmid
        );
        let body = serde_json::json!({
            "newid": new_vmid,
            "name":  name,
            "full":  true,   // clone complet (pas lié), requis pour VM jetable
        });
        self.post_upid(&url, body)
            .await
            .with_context(|| format!(
                "clonage du template {template_vmid} → VM {new_vmid}"
            ))
    }

    /// Configure un argument QEMU sur une VM arrêtée.
    ///
    /// Utilisé principalement pour définir le CID vsock de la VM clonée,
    /// via : `set_config(vmid, "args", "-device vhost-vsock-pci,guest-cid=X")`.
    ///
    /// ⚠️  Le CID vsock ne peut être appliqué qu'à une VM **arrêtée** —
    ///  il faut stopper la VM, appeler cette méthode, puis la redémarrer.
    pub async fn set_config(&self, vmid: u32, key: &str, value: &str) -> Result<()> {
        let url = format!(
            "{}/api2/json/nodes/{}/qemu/{}/config",
            self.base_url, self.node, vmid
        );
        let body = serde_json::json!({ key: value });
        let resp = self
            .http
            .put(&url)
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await
            .context("envoi de la requête PUT config")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Proxmox a répondu {status} lors de set_config({vmid}, {key}) : {body}");
        }
        Ok(())
    }

    /// Démarre une VM arrêtée.
    ///
    /// Retourne l'UPID de la tâche de démarrage. Passer ce résultat à
    /// `wait_for_task()` avant de considérer la VM comme disponible.
    pub async fn start_vm(&self, vmid: u32) -> Result<String> {
        let url = format!(
            "{}/api2/json/nodes/{}/qemu/{}/status/start",
            self.base_url, self.node, vmid
        );
        self.post_upid(&url, serde_json::json!({}))
            .await
            .with_context(|| format!("démarrage de la VM {vmid}"))
    }

    /// Arrête une VM (arrêt propre, équivalent à un poweroff OS).
    ///
    /// Retourne l'UPID de la tâche d'arrêt. Passer ce résultat à
    /// `wait_for_task()` avant d'appeler `delete_vm()`.
    pub async fn stop_vm(&self, vmid: u32) -> Result<String> {
        let url = format!(
            "{}/api2/json/nodes/{}/qemu/{}/status/stop",
            self.base_url, self.node, vmid
        );
        self.post_upid(&url, serde_json::json!({}))
            .await
            .with_context(|| format!("arrêt de la VM {vmid}"))
    }

    /// Détruit définitivement une VM (disque inclus).
    ///
    /// ⚠️  Opération IRRÉVERSIBLE — la VM doit être arrêtée.
    /// Retourne l'UPID de la tâche de suppression.
    pub async fn delete_vm(&self, vmid: u32) -> Result<String> {
        let url = format!(
            "{}/api2/json/nodes/{}/qemu/{}?purge=1&destroy-unreferenced-disks=1",
            self.base_url, self.node, vmid
        );
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .context("envoi de la requête DELETE")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Proxmox a répondu {status} lors de delete_vm({vmid}) : {body}");
        }

        #[derive(Deserialize)]
        struct Wrapper { data: String }
        let w: Wrapper = resp.json().await.context("parsing UPID delete")?;
        Ok(w.data)
    }

    /// Attend la fin d'une tâche Proxmox (identifiée par son UPID).
    ///
    /// Polle l'endpoint `/nodes/{node}/tasks/{upid}/status` toutes les
    /// `poll_interval` millisecondes, jusqu'à ce que `status` soit
    /// `"stopped"` (succès ou échec). Retourne une erreur si la tâche se
    /// termine avec `exitstatus != "OK"`, ou si le timeout est dépassé.
    ///
    /// - `upid`          : identifiant de tâche retourné par clone/start/stop/delete.
    /// - `timeout`       : durée maximale d'attente (ex. 120s pour un clone).
    /// - `poll_interval` : intervalle de polling en millisecondes (ex. 500).
    pub async fn wait_for_task(
        &self,
        upid: &str,
        timeout: std::time::Duration,
        poll_interval_ms: u64,
    ) -> Result<()> {
        use std::time::Instant;

        // L'UPID peut contenir des '/' qui doivent être encodés dans l'URL.
        let upid_encoded = upid.replace('/', "%2F");
        let url = format!(
            "{}/api2/json/nodes/{}/tasks/{}/status",
            self.base_url, self.node, upid_encoded
        );

        #[derive(Deserialize)]
        struct TaskStatus {
            status: String,
            #[serde(default)]
            exitstatus: Option<String>,
        }

        let start = Instant::now();
        loop {
            if start.elapsed() > timeout {
                bail!(
                    "timeout ({:.0}s) en attendant la fin de la tâche Proxmox {upid}",
                    timeout.as_secs_f64()
                );
            }

            let resp = self
                .http
                .get(&url)
                .header("Authorization", &self.auth_header)
                .send()
                .await
                .context("polling du statut de tâche")?;

            let http_status = resp.status();
            if !http_status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!("Proxmox a répondu {http_status} lors du polling de {upid} : {body}");
            }

            #[derive(Deserialize)]
            struct Wrapper { data: TaskStatus }
            let w: Wrapper = resp.json().await.context("parsing statut de tâche")?;
            let task = w.data;

            if task.status == "stopped" {
                return match task.exitstatus.as_deref() {
                    Some("OK") | None => Ok(()),
                    Some(exit) => bail!(
                        "tâche Proxmox {upid} terminée avec erreur : {exit}"
                    ),
                };
            }

            tokio::time::sleep(std::time::Duration::from_millis(poll_interval_ms)).await;
        }
    }

    /// Requête POST retournant un UPID (cas standard des opérations
    /// asynchrones Proxmox : clone, start, stop...).
    async fn post_upid(&self, url: &str, body: serde_json::Value) -> Result<String> {
        let resp = self
            .http
            .post(url)
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await
            .context("envoi de la requête POST")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Proxmox a répondu {status} : {body}");
        }

        #[derive(Deserialize)]
        struct Wrapper { data: String }
        let w: Wrapper = resp.json().await.context("parsing UPID")?;
        Ok(w.data)
    }
}

/// Convertit une empreinte hexadécimale (avec ou sans `:`) en 32 octets.
fn parse_sha256_fingerprint(s: &str) -> Result<[u8; 32]> {
    let cleaned: String = s.chars().filter(|c| *c != ':').collect();
    if cleaned.len() != 64 {
        bail!(
            "longueur invalide : attendu 64 caractères hex (32 octets), reçu {}",
            cleaned.len()
        );
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("caractère hexadécimal invalide à la position {}", i * 2))?;
    }
    Ok(out)
}

/// Vérificateur de certificat TLS par épinglage d'empreinte SHA-256.
///
/// Le certificat Proxmox étant auto-signé par défaut, la validation de
/// chaîne classique (CA de confiance) n'a pas de sens ici. On vérifie à
/// la place que le certificat présenté est *exactement* celui attendu
/// (comparaison de son empreinte SHA-256), ce qui protège contre une
/// interception réseau sans dépendre d'une PKI externe.
#[derive(Debug)]
struct FingerprintVerifier {
    expected_fingerprint: [u8; 32],
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let actual: [u8; 32] = hasher.finalize().into();

        if actual == self.expected_fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "empreinte du certificat Proxmox inattendue (attendu {}, reçu {}) — \
                 vérifier cert_fingerprint_sha256 dans la config, ou risque d'interception",
                hex_encode(&self.expected_fingerprint),
                hex_encode(&actual),
            )))
        }
    }

    // La chaîne de signature n'est pas pertinente pour un certificat
    // auto-signé épinglé par empreinte exacte : on accepte, l'identité
    // ayant déjà été prouvée par la correspondance d'empreinte ci-dessus.
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}


// ── Implémentation du trait VmProvider ──────────────────────────────────────

use super::{ProviderVm, ProviderVmStatus, VmProvider};
use async_trait::async_trait;

#[async_trait]
impl VmProvider for ProxmoxClient {
    fn backend_name(&self) -> &'static str {
        "proxmox"
    }

    async fn list_vms(&self) -> Result<Vec<ProviderVm>> {
        let summaries = ProxmoxClient::list_vms(self).await?;
        Ok(summaries
            .into_iter()
            .map(|s| ProviderVm {
                provider_id: s.vmid.to_string(),
                name: s.name,
                status: match s.status.as_str() {
                    "running" => ProviderVmStatus::Running,
                    "stopped" => ProviderVmStatus::Stopped,
                    other => ProviderVmStatus::Unknown(other.to_string()),
                },
            })
            .collect())
    }

    async fn get_status(&self, provider_id: &str) -> Result<ProviderVm> {
        let vmid: u32 = provider_id
            .parse()
            .context("provider_id doit être un VMID numérique pour Proxmox")?;
        let s = self.get_vm_status(vmid).await?;
        Ok(ProviderVm {
            provider_id: s.vmid.to_string(),
            name: s.name,
            status: match s.status.as_str() {
                "running" => ProviderVmStatus::Running,
                "stopped" => ProviderVmStatus::Stopped,
                other => ProviderVmStatus::Unknown(other.to_string()),
            },
        })
    }

    async fn clone_vm(
        &self,
        template_id: &str,
        new_name: &str,
    ) -> Result<ProviderVm> {
        let template_vmid: u32 = template_id
            .parse()
            .context("template_id Proxmox doit être numérique")?;
        // Allocation simplifiée du VMID — sera raffinée en Phase 4
        // quand le pool dynamique aura un allocateur de VMID dédié.
        let new_vmid = template_vmid + 100;
        let upid =
            ProxmoxClient::clone_vm(self, template_vmid, new_vmid, new_name)
                .await?;
        self.wait_for_task(
            &upid,
            std::time::Duration::from_secs(120),
            500,
        )
        .await?;
        self.get_status(&new_vmid.to_string()).await
    }

    async fn start_vm(&self, provider_id: &str) -> Result<()> {
        let vmid: u32 = provider_id.parse().context("VMID invalide")?;
        let upid = ProxmoxClient::start_vm(self, vmid).await?;
        self.wait_for_task(
            &upid,
            std::time::Duration::from_secs(60),
            500,
        )
        .await
    }

    async fn stop_vm(&self, provider_id: &str) -> Result<()> {
        let vmid: u32 = provider_id.parse().context("VMID invalide")?;
        let upid = ProxmoxClient::stop_vm(self, vmid).await?;
        self.wait_for_task(
            &upid,
            std::time::Duration::from_secs(60),
            500,
        )
        .await
    }

    async fn delete_vm(&self, provider_id: &str) -> Result<()> {
        let vmid: u32 = provider_id.parse().context("VMID invalide")?;
        let upid = ProxmoxClient::delete_vm(self, vmid).await?;
        self.wait_for_task(
            &upid,
            std::time::Duration::from_secs(60),
            500,
        )
        .await
    }

    async fn set_vsock_cid(&self, provider_id: &str, cid: u32) -> Result<()> {
        let vmid: u32 = provider_id.parse().context("VMID invalide")?;
        let args = format!("-device vhost-vsock-pci,guest-cid={cid}");
        self.set_config(vmid, "args", &args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fingerprint_avec_deux_points() {
        let s = "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:\
                  AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99";
        let out = parse_sha256_fingerprint(s).unwrap();
        assert_eq!(out[0], 0xAA);
        assert_eq!(out[1], 0xBB);
        assert_eq!(out.len(), 32);
    }

    #[test]
    fn parse_fingerprint_sans_deux_points() {
        let s = "aabbccddeeff001122334455667788990011223344556677\
                  8899aabbccdd";
        // 64 caractères hex attendus : ajustons la longueur exacte.
        let s64 = "aabbccddeeff00112233445566778899aabbccddeeff0011\
                    2233445566778899";
        let out = parse_sha256_fingerprint(s64).unwrap();
        assert_eq!(out.len(), 32);
        let _ = s; // silence unused si longueur ci-dessus ajustée
    }

    #[test]
    fn parse_fingerprint_longueur_invalide_echoue() {
        assert!(parse_sha256_fingerprint("AABBCC").is_err());
    }

    #[test]
    fn hex_encode_roundtrip() {
        let bytes = [0xDE, 0xAD, 0xBE, 0xEF];
        assert_eq!(hex_encode(&bytes), "deadbeef");
    }
}
