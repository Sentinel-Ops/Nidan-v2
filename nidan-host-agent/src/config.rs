//! Configuration du nidan-host-agent.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Configuration principale de l'agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostAgentConfig {
    /// Configuration du transport vsock.
    #[serde(default)]
    pub vsock: VsockConfig,
    /// Configuration libvirt.
    pub libvirt: LibvirtConfig,
    /// Configuration de sécurité.
    #[serde(default)]
    pub security: SecurityConfig,
}

/// Transport vsock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VsockConfig {
    /// Port d'écoute vsock.
    #[serde(default = "default_vsock_port")]
    pub port: u32,
    /// CID autorisé à se connecter (le broker). None = accepter tout.
    pub allowed_cid: Option<u32>,
}

fn default_vsock_port() -> u32 { 6900 }

impl Default for VsockConfig {
    fn default() -> Self {
        Self {
            port: default_vsock_port(),
            allowed_cid: None,
        }
    }
}

/// Configuration libvirt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibvirtConfig {
    /// URI de connexion libvirt (ex: "qemu:///system").
    #[serde(default = "default_libvirt_uri")]
    pub uri: String,
    /// Nom du pool de stockage pour les clones.
    #[serde(default = "default_storage_pool")]
    pub storage_pool: String,
    /// Préfixe obligatoire pour les noms de VMs NIDAN.
    #[serde(default = "default_vm_prefix")]
    pub vm_prefix: String,
}

fn default_libvirt_uri()  -> String { "qemu:///system".to_string() }
fn default_storage_pool() -> String { "default".to_string() }
fn default_vm_prefix()    -> String { "nidan-".to_string() }

impl Default for LibvirtConfig {
    fn default() -> Self {
        Self {
            uri: default_libvirt_uri(),
            storage_pool: default_storage_pool(),
            vm_prefix: default_vm_prefix(),
        }
    }
}

/// Sécurité et audit.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecurityConfig {
    /// Chemin du fichier d'audit (optionnel).
    pub audit_log: Option<String>,
}

impl HostAgentConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("lecture {path}"))?;
        toml::from_str(&content).context("parsing TOML config host-agent")
    }

    pub fn validate(&self) -> Result<()> {
        if self.libvirt.vm_prefix.is_empty() {
            bail!("libvirt.vm_prefix ne peut pas être vide");
        }
        if self.libvirt.uri.is_empty() {
            bail!("libvirt.uri ne peut pas être vide");
        }
        Ok(())
    }
}
