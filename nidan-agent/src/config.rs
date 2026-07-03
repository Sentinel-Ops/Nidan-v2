//! Configuration de nidan-agent.
//!
//! Beaucoup plus légère que celle du serveur v1 : l'agent n'a pas de TLS,
//! pas de JWT, pas de gestion de session, pas de presse-papier.
//! Il configure uniquement :
//!   • la sortie vsock (port sur l'hôte, CID cible)
//!   • le backend de capture (stub / pipewire)
//!   • le mode d'injection des entrées

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    pub vsock:   VsockConfig,
    pub capture: CaptureConfig,
    #[serde(default)]
    pub input:   InputConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VsockConfig {
    /// CID de l'hôte cible. Par convention vsock, l'hôte est toujours 2.
    /// N'a de sens que si on veut faire du testing local (CID 1 = loopback).
    #[serde(default = "default_host_cid")]
    pub host_cid: u32,

    /// Port vsock sur lequel le proxy-encoder écoute côté hôte.
    #[serde(default = "default_port")]
    pub port: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CaptureConfig {
    /// Backend de capture. Valeurs possibles :
    ///   "stub"     — pattern factice (dev/test)
    ///   "pipewire" — vraie capture Wayland via portail ScreenCast
    pub backend: String,

    /// Cadence maximum de la capture (frames/s).
    #[serde(default = "default_fps")]
    pub max_fps: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct InputConfig {
    /// Backend d'injection. Valeurs possibles :
    ///   "none"          — l'agent ignore les entrées (dev/test)
    ///   "remotedesktop" — portail Wayland RemoteDesktop
    ///   "x11"           — XTEST (fallback X11, rare en v2)
    #[serde(default = "default_input_backend")]
    pub backend: String,
}

fn default_host_cid() -> u32 { 2 }
fn default_port() -> u32     { 6100 }
fn default_fps() -> u32      { 30 }
fn default_input_backend() -> String { "remotedesktop".to_string() }

impl AgentConfig {
    /// Charge la config depuis un fichier TOML.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("lecture de {}", path.display()))?;
        let cfg: AgentConfig = toml::from_str(&raw)
            .with_context(|| format!("parsing TOML de {}", path.display()))?;
        Ok(cfg)
    }
}
