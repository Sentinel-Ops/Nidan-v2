//! Protocole broker ↔ host-agent (canal 2 — vsock).
//!
//! Définit les messages échangés entre le broker (VM, CID 3) et le
//! nidan-host-agent (socle) via AF_VSOCK.
//!
//! ## Transport
//!
//! - Chaque message est sérialisé en JSON, préfixé par sa longueur (4 bytes
//!   big-endian). Voir [`super::encode_message`] / [`super::decode_message`].
//! - Une connexion vsock par requête (pas de multiplexing).
//!
//! ## Opérations supportées
//!
//! | Action          | Description                                  |
//! |-----------------|----------------------------------------------|
//! | `list_vms`      | Liste les VMs gérées (filtrées par préfixe)  |
//! | `get_status`    | Statut d'une VM (par UUID ou nom)            |
//! | `clone_vm`      | Clone un template en nouvelle VM             |
//! | `start_vm`      | Démarre une VM arrêtée                       |
//! | `stop_vm`       | Arrête une VM (shutdown propre, puis destroy) |
//! | `delete_vm`     | Supprime une VM et ses volumes               |
//! | `set_vsock_cid` | Configure le CID vsock d'une VM              |

use serde::{Deserialize, Serialize};

/// Port vsock par défaut pour le host-agent.
pub const HOST_AGENT_DEFAULT_PORT: u32 = 6900;

/// CID de l'hôte (convention vsock : le host est toujours CID 2).
pub const VMADDR_CID_HOST: u32 = 2;

// ── Requêtes (broker → agent) ───────────────────────────────────────────────

/// Requête envoyée par le broker au host-agent.
///
/// Le `#[serde(tag = "action")]` permet au JSON d'avoir un champ `"action"`
/// qui détermine le variant, ce qui rend le protocole explicite et debuggable :
///
/// ```json
/// { "action": "clone_vm", "template": "nidan-template", "new_name": "" }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AgentRequest {
    /// Liste les VMs dont le nom commence par `prefix`.
    ListVms {
        prefix: String,
    },

    /// Récupère le statut d'une VM (par UUID ou nom).
    GetStatus {
        vm_id: String,
    },

    /// Clone un template en une nouvelle VM.
    /// Si `new_name` est vide, l'agent génère un nom unique.
    CloneVm {
        template: String,
        new_name: String,
    },

    /// Démarre une VM arrêtée.
    StartVm {
        vm_id: String,
    },

    /// Arrête une VM (shutdown propre, puis destroy si timeout).
    StopVm {
        vm_id: String,
    },

    /// Supprime une VM et ses volumes disque.
    DeleteVm {
        vm_id: String,
    },

    /// Configure le CID vsock d'une VM.
    SetVsockCid {
        vm_id: String,
        cid: u32,
    },
}

// ── Réponses (agent → broker) ───────────────────────────────────────────────

/// Réponse du host-agent au broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    /// `true` si l'opération a réussi.
    pub success: bool,

    /// Résultat de l'opération (structure dépend de l'action).
    /// - `list_vms`  → `Vec<AgentVm>` sérialisé
    /// - `get_status` / `clone_vm` → `AgentVm` sérialisé
    /// - `start_vm` / `stop_vm` / `delete_vm` / `set_vsock_cid` → `null`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,

    /// Message d'erreur si `success` est `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AgentResponse {
    /// Crée une réponse de succès sans résultat (opérations void).
    pub fn ok_empty() -> Self {
        Self {
            success: true,
            result: None,
            error: None,
        }
    }

    /// Crée une réponse de succès avec un résultat sérialisable.
    pub fn ok_with<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        Ok(Self {
            success: true,
            result: Some(serde_json::to_value(value)?),
            error: None,
        })
    }

    /// Crée une réponse d'erreur.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            result: None,
            error: Some(message.into()),
        }
    }

    /// Extrait le résultat typé si success, sinon retourne l'erreur.
    pub fn into_result<T: serde::de::DeserializeOwned>(self) -> Result<T, String> {
        if !self.success {
            return Err(self.error.unwrap_or_else(|| "erreur inconnue".into()));
        }
        let val = self.result.ok_or_else(|| "résultat absent".to_string())?;
        serde_json::from_value(val).map_err(|e| format!("désérialisation résultat: {e}"))
    }

    /// Vérifie le succès pour les opérations sans résultat.
    pub fn into_unit_result(self) -> Result<(), String> {
        if self.success {
            Ok(())
        } else {
            Err(self.error.unwrap_or_else(|| "erreur inconnue".into()))
        }
    }
}

// ── VM côté agent ───────────────────────────────────────────────────────────

/// Description d'une VM retournée par l'agent.
///
/// Équivalent simplifié de `ProviderVm` côté broker — ne contient que
/// les informations que l'agent expose (pas de détails libvirt internes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentVm {
    /// Identifiant unique du provider (UUID libvirt).
    pub provider_id: String,
    /// Nom du domaine libvirt.
    pub name: String,
    /// État : "running", "stopped", ou description libre.
    pub status: String,
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialize_clone() {
        let req = AgentRequest::CloneVm {
            template: "nidan-template".into(),
            new_name: "".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""action":"clone_vm""#));
        assert!(json.contains(r#""template":"nidan-template""#));
    }

    #[test]
    fn test_request_deserialize_start() {
        let json = r#"{"action":"start_vm","vm_id":"uuid-123"}"#;
        let req: AgentRequest = serde_json::from_str(json).unwrap();
        match req {
            AgentRequest::StartVm { vm_id } => assert_eq!(vm_id, "uuid-123"),
            _ => panic!("attendu StartVm"),
        }
    }

    #[test]
    fn test_request_serialize_set_vsock_cid() {
        let req = AgentRequest::SetVsockCid {
            vm_id: "uuid-456".into(),
            cid: 42,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""cid":42"#));
    }

    #[test]
    fn test_response_ok_empty() {
        let resp = AgentResponse::ok_empty();
        assert!(resp.success);
        assert!(resp.result.is_none());
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("result"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn test_response_ok_with_vm() {
        let vm = AgentVm {
            provider_id: "uuid-789".into(),
            name: "nidan-a1b2".into(),
            status: "running".into(),
        };
        let resp = AgentResponse::ok_with(&vm).unwrap();
        assert!(resp.success);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("nidan-a1b2"));
    }

    #[test]
    fn test_response_ok_with_vm_list() {
        let vms = vec![
            AgentVm { provider_id: "u1".into(), name: "nidan-vm1".into(), status: "running".into() },
            AgentVm { provider_id: "u2".into(), name: "nidan-vm2".into(), status: "stopped".into() },
        ];
        let resp = AgentResponse::ok_with(&vms).unwrap();
        let recovered: Vec<AgentVm> = resp.into_result().unwrap();
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0].name, "nidan-vm1");
        assert_eq!(recovered[1].status, "stopped");
    }

    #[test]
    fn test_response_error() {
        let resp = AgentResponse::err("template introuvable");
        assert!(!resp.success);
        assert_eq!(resp.error.as_deref(), Some("template introuvable"));
        let result: Result<AgentVm, _> = resp.into_result();
        assert!(result.is_err());
    }

    #[test]
    fn test_response_into_unit_result() {
        let ok = AgentResponse::ok_empty();
        assert!(ok.into_unit_result().is_ok());

        let fail = AgentResponse::err("nope");
        assert_eq!(fail.into_unit_result(), Err("nope".to_string()));
    }

    #[test]
    fn test_roundtrip_encode_decode() {
        let req = AgentRequest::ListVms { prefix: "nidan-".into() };
        let encoded = super::super::encode_message(&req).unwrap();
        let decoded: AgentRequest = super::super::decode_message(&encoded[4..]).unwrap();
        match decoded {
            AgentRequest::ListVms { prefix } => assert_eq!(prefix, "nidan-"),
            _ => panic!("attendu ListVms"),
        }
    }

    #[test]
    fn test_all_actions_deserialize() {
        // Vérifie que chaque action se désérialise correctement
        let cases = vec![
            r#"{"action":"list_vms","prefix":"nidan-"}"#,
            r#"{"action":"get_status","vm_id":"abc"}"#,
            r#"{"action":"clone_vm","template":"t","new_name":"n"}"#,
            r#"{"action":"start_vm","vm_id":"abc"}"#,
            r#"{"action":"stop_vm","vm_id":"abc"}"#,
            r#"{"action":"delete_vm","vm_id":"abc"}"#,
            r#"{"action":"set_vsock_cid","vm_id":"abc","cid":10}"#,
        ];
        for json in cases {
            let req: AgentRequest = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("échec désérialisation: {json} → {e}"));
            // Vérifie le round-trip
            let re_json = serde_json::to_string(&req).unwrap();
            let _: AgentRequest = serde_json::from_str(&re_json).unwrap();
        }
    }
}
