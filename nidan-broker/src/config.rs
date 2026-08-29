//! Configuration du broker NIDAN.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use nidan_common::config::TlsConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerConfig {
    pub network:  NetworkConfig,
    pub auth:     AuthConfig,
    pub pool:     PoolConfig,
    pub tls:      TlsConfig,
    pub security: SecurityConfig,
    pub admin:    AdminConfig,
    #[serde(default)]
    pub provider: ProviderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Adresse QUIC publique (clients)
    #[serde(default = "default_quic_bind")]
    pub quic_bind: String,
    /// Adresse HTTP admin (interne)
    #[serde(default = "default_admin_bind")]
    pub admin_bind: String,
    /// Timeout session inactive (secondes)
    #[serde(default = "default_session_timeout")]
    pub session_timeout_secs: u64,
    /// Nombre max de sessions simultanées
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// Adresse du proxy-encoder retournée aux clients.
    /// Si absent, le broker retourne l'adresse IP directe de la VM.
    pub proxy_address: Option<String>,
}

fn default_quic_bind()      -> String { "0.0.0.0:7443".to_string() }
fn default_admin_bind()     -> String { "127.0.0.1:7080".to_string() }
fn default_session_timeout() -> u64   { 3600 }
fn default_max_sessions()   -> usize  { 100 }

/// Configuration de l'authentification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Méthodes d'auth activées
    #[serde(default = "default_auth_methods")]
    pub enabled_methods: Vec<String>,

    /// Configuration JWT (sessions broker)
    pub jwt: JwtConfig,

    /// Configuration Kerberos (optionnel)
    pub kerberos: Option<KerberosConfig>,

    /// Configuration OIDC (optionnel)
    pub oidc: Option<OidcConfig>,

    /// Durée de vie d'un session token (secondes)
    #[serde(default = "default_token_ttl")]
    pub session_token_ttl_secs: u64,
}

fn default_auth_methods() -> Vec<String> {
    vec!["mtls".to_string()]
}
fn default_token_ttl() -> u64 { 300 } // 5 minutes

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtConfig {
    /// Clé secrète HMAC-SHA256 pour signer les session tokens
    pub secret: String,
    /// Issuer JWT
    #[serde(default = "default_issuer")]
    pub issuer: String,
}

fn default_issuer() -> String { "nidan-broker".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KerberosConfig {
    /// Keytab du service NIDAN
    pub keytab_path: String,
    /// Principal du service (ex: nidan/broker.interne.fr@INTERNE.FR)
    pub service_principal: String,
    /// Realm Kerberos
    pub realm: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcConfig {
    /// URL du fournisseur OIDC (ex: https://sso.interne.fr)
    pub issuer_url: String,
    /// Client ID
    pub client_id: String,
    /// Audience attendue dans le token
    pub audience: String,
    /// Attribut du token utilisé comme identité (ex: "sub", "email")
    #[serde(default = "default_identity_claim")]
    pub identity_claim: String,
}

fn default_identity_claim() -> String { "sub".to_string() }

/// Configuration du pool de VMs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// VMs statiques déclarées (mode simple)
    #[serde(default)]
    pub static_vms: Vec<VmEntry>,
    /// Taille minimale du pool de VMs disponibles
    #[serde(default = "default_pool_min")]
    pub min_available: usize,
    /// Timeout de health check des VMs (secondes)
    #[serde(default = "default_health_timeout")]
    pub health_check_timeout_secs: u64,
    /// Intervalle entre health checks (secondes)
    #[serde(default = "default_health_interval")]
    pub health_check_interval_secs: u64,
    /// Configuration du pool dynamique (optionnel)
    #[serde(default)]
    pub dynamic: Option<DynamicPoolConfig>,
}

fn default_pool_min()         -> usize { 1 }
fn default_health_timeout()   -> u64   { 5 }
fn default_health_interval()  -> u64   { 30 }

/// Configuration du pool dynamique (provisionnement automatique de VMs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicPoolConfig {
    /// Nom ou UUID du template libvirt à cloner
    pub template: String,
    /// Port QUIC du serveur NIDAN dans les VMs clonées
    #[serde(default = "default_vm_port")]
    pub vm_port: u16,
    /// Début de la plage CID vsock (inclus)
    #[serde(default = "default_cid_start")]
    pub cid_start: u32,
    /// Fin de la plage CID vsock (inclus)
    #[serde(default = "default_cid_end")]
    pub cid_end: u32,
    /// Pattern d'adresse IP des VMs dynamiques.
    /// `{cid}` sera remplacé par le CID alloué.
    #[serde(default = "default_vm_ip_pattern")]
    pub vm_ip_pattern: String,
}

fn default_vm_port()       -> u16    { 7444 }
fn default_cid_start()     -> u32    { 10 }
fn default_cid_end()       -> u32    { 99 }
fn default_vm_ip_pattern() -> String { "192.168.122.{cid}".to_string() }

/// Entrée de VM dans la configuration statique
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmEntry {
    pub id:   String,
    pub host: String,
    pub port: u16,
    /// Tags pour le filtrage (ex: ["gpu", "windows"])
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Configuration de sécurité du broker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Activer le rate limiting sur les tentatives d'auth
    #[serde(default = "default_true")]
    pub rate_limit_auth: bool,
    /// Nombre max de tentatives d'auth par IP / 5 minutes
    #[serde(default = "default_max_auth_attempts")]
    pub max_auth_attempts: u32,
    /// Bannir une IP après max_auth_attempts (secondes)
    #[serde(default = "default_ban_duration")]
    pub ban_duration_secs: u64,
    /// Journaliser toutes les tentatives d'auth (succès + échecs)
    #[serde(default = "default_true")]
    pub audit_all_auth: bool,
}

fn default_true()              -> bool { true }
fn default_max_auth_attempts() -> u32  { 5 }
fn default_ban_duration()      -> u64  { 300 }

/// Configuration de l'API admin
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminConfig {
    /// Token statique pour l'API admin (à remplacer par mTLS en prod)
    pub admin_token: Option<String>,
    /// Activer le endpoint /metrics (Prometheus)
    #[serde(default = "default_true")]
    pub metrics_enabled: bool,
}


/// Configuration du provider d'infrastructure VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Backend : "static" (défaut), "proxmox", "libvirt"
    #[serde(default = "default_provider_backend")]
    pub backend: String,
    /// Configuration libvirt (requise si backend = "libvirt")
    pub libvirt: Option<LibvirtProviderConfig>,
    /// Configuration host-agent vsock (requise si backend = "host-agent")
    pub host_agent: Option<HostAgentProviderConfig>,
}

fn default_provider_backend() -> String { "static".to_string() }

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            backend: default_provider_backend(),
            libvirt: None,
            host_agent: None,
        }
    }
}

/// Configuration spécifique au backend libvirt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibvirtProviderConfig {
    /// URI de connexion libvirt (ex: "qemu:///system", "qemu+ssh://root@host/system")
    #[serde(default = "default_libvirt_uri")]
    pub uri: String,
    /// Nom du pool de stockage pour les clones de volumes
    #[serde(default = "default_libvirt_pool")]
    pub storage_pool: String,
    /// Préfixe des noms de VMs gérées par NIDAN
    #[serde(default = "default_vm_prefix")]
    pub vm_prefix: String,
}

fn default_libvirt_uri()  -> String { "qemu:///system".to_string() }
fn default_libvirt_pool() -> String { "default".to_string() }
fn default_vm_prefix()    -> String { "nidan-".to_string() }

impl Default for LibvirtProviderConfig {
    fn default() -> Self {
        Self {
            uri:          default_libvirt_uri(),
            storage_pool: default_libvirt_pool(),
            vm_prefix:    default_vm_prefix(),
        }
    }
}

/// Configuration spécifique au backend host-agent (vsock).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostAgentProviderConfig {
    /// CID de l'hôte (convention vsock : toujours 2)
    #[serde(default = "default_host_cid")]
    pub host_cid: u32,
    /// Port vsock du nidan-host-agent
    #[serde(default = "default_agent_port")]
    pub port: u32,
    /// Préfixe des noms de VMs NIDAN (doit matcher la config de l'agent)
    #[serde(default = "default_vm_prefix")]
    pub vm_prefix: String,
    /// Timeout de connexion vsock (secondes)
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
}

fn default_host_cid()        -> u32    { 2 }
fn default_agent_port()      -> u32    { 6900 }
fn default_connect_timeout() -> u64    { 5 }

impl Default for HostAgentProviderConfig {
    fn default() -> Self {
        Self {
            host_cid: default_host_cid(),
            port: default_agent_port(),
            vm_prefix: default_vm_prefix(),
            connect_timeout_secs: default_connect_timeout(),
        }
    }
}

impl BrokerConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("lecture {}", path.as_ref().display()))?;
        toml::from_str(&content).context("parsing TOML config broker")
    }

    pub fn validate(&self) -> Result<()> {
        if self.network.quic_bind.is_empty() {
            bail!("network.quic_bind vide");
        }
        if self.auth.jwt.secret.len() < 32 {
            bail!("auth.jwt.secret trop court (minimum 32 caractères)");
        }
        if !Path::new(&self.tls.ca_cert).exists() {
            bail!("tls.ca_cert introuvable: {}", self.tls.ca_cert);
        }
        for vm in &self.pool.static_vms {
            if vm.id.is_empty() || vm.host.is_empty() {
                bail!("VM invalide dans pool.static_vms");
            }
        }
        // Validation du provider
        match self.provider.backend.as_str() {
            "static" | "proxmox" | "libvirt" | "host-agent" => {}
            other => bail!("provider.backend inconnu: {other} (attendu: static, proxmox, libvirt)"),
        }
        if self.provider.backend == "libvirt" && self.provider.libvirt.is_none() {
            bail!("provider.backend=libvirt mais section [provider.libvirt] absente");
        }
        if self.provider.backend == "host-agent" && self.provider.host_agent.is_none() {
            bail!("provider.backend=host-agent mais section [provider.host_agent] absente");
        }
        Ok(())
    }
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig {
                quic_bind:            default_quic_bind(),
                admin_bind:           default_admin_bind(),
                session_timeout_secs: default_session_timeout(),
                max_sessions:         default_max_sessions(),
                proxy_address:        None,
            },
            auth: AuthConfig {
                enabled_methods:      default_auth_methods(),
                jwt: JwtConfig {
                    secret: "CHANGEME_minimum_32_chars_secret!".to_string(),
                    issuer: default_issuer(),
                },
                kerberos:             None,
                oidc:                 None,
                session_token_ttl_secs: default_token_ttl(),
            },
            pool: PoolConfig {
                static_vms:                  vec![],
                min_available:               default_pool_min(),
                health_check_timeout_secs:   default_health_timeout(),
                health_check_interval_secs:  default_health_interval(),
                dynamic:                     None,
            },
            tls: TlsConfig {
                ca_cert: "/etc/nidan/certs/ca.crt".to_string(),
                cert:    "/etc/nidan/certs/broker.crt".to_string(),
                key:     "/etc/nidan/certs/broker.key".to_string(),
                allowed_client_domains: vec![],
            },
            security: SecurityConfig {
                rate_limit_auth:     true,
                max_auth_attempts:   default_max_auth_attempts(),
                ban_duration_secs:   default_ban_duration(),
                audit_all_auth:      true,
            },
            admin: AdminConfig {
                admin_token:     None,
                metrics_enabled: true,
            },
            provider: ProviderConfig::default(),
        }
    }
}
