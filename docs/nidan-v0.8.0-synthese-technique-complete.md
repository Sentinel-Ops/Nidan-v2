# NIDAN v2 — Synthèse technique complète

## Sessions 28-30 août 2026

> **Auteur :** Jean-Philippe JUBEBNOT  
> **Projet :** NIDAN v2 — Bureau distant sécurisé  
> **Repo :** https://github.com/Sentinel-Ops/Nidan-v2  
> **Tag précédent :** v0.7.3-client-perf  
> **Tag cible :** v0.8.0-dynamic-pool

---

## Table des matières

1. [Vue d'ensemble de l'architecture](#1-vue-densemble)
2. [Trait VmProvider et abstraction des backends](#2-trait-vmprovider)
3. [LibvirtProvider — backend de développement](#3-libvirtprovider)
4. [Pool dynamique — CidAllocator et provisionnement](#4-pool-dynamique)
5. [Canal 2 — pilotage hyperviseur via vsock](#5-canal-2)
6. [JWT CID et routage proxy](#6-jwt-cid)
7. [Multi-VM — routage frames et inputs par CID](#7-multi-vm)
8. [Destruction immédiate à la déconnexion](#8-destruction-immédiate)
9. [Thin clone — backing file qcow2](#9-thin-clone)
10. [Analyse de sécurité](#10-sécurité)
11. [Tests end-to-end validés](#11-tests)
12. [Points restants à traiter](#12-points-restants)

---

## 1. Vue d'ensemble

### Architecture finale

```
                          Réseau externe (non fiable)
                               │
                          QUIC/mTLS
                               │
┌──────────────────────────────┼──────────────────────────────────┐
│  Socle KVM (Dell R440)       │        PÉRIMÈTRE DE CONFIANCE   │
│                              │                                  │
│  ┌───────────────────────────┴──────────┐                       │
│  │  VM Broker (CID 3)                   │                       │
│  │  • Auth mTLS + MFA                   │                       │
│  │  • Pool VMs (statique + dynamique)   │                       │
│  │  • Signe JWT { cid, vm_id, proxy }   │                       │
│  └──────────┬───────────────────────────┘                       │
│             │ vsock :6900 (canal 2)                              │
│             ▼                                                    │
│  ┌──────────────────────────────────────┐                       │
│  │  Host-Agent (sur le socle)           │                       │
│  │  • Écoute vsock :6900                │                       │
│  │  • CID 3 = broker (toutes ops)       │                       │
│  │  • CID 1 = proxy (stop/delete)       │                       │
│  │  • Filtrage préfixe nidan-*          │                       │
│  │  • Audit chaque opération            │                       │
│  │  → libvirt (clone/start/stop/delete) │                       │
│  └──────────────────────────────────────┘                       │
│                                                                  │
│  ┌──────────────────────────────────────┐                       │
│  │  Proxy-Encoder (sur le socle)        │                       │
│  │  • QUIC/mTLS :7610                   │                       │
│  │  • Vérifie JWT, extrait CID          │                       │
│  │  • Routage frames par CID source     │                       │
│  │  • Routage inputs par CID dédié      │                       │
│  │  • Encode H.264 (openh264)           │                       │
│  │  • E2E crypto (X25519+ChaCha20)      │                       │
│  │  • Cleanup VM via host-agent (CID 1) │                       │
│  └───────┬──────────┬───────────────────┘                       │
│          │          │                                            │
│     vsock│:6100     │vsock :6100                                 │
│          │          │                                            │
│  ┌───────┴───┐  ┌───┴───────┐  ┌─────────────┐                 │
│  │ VM Desktop│  │ VM Desktop│  │ VM Desktop  │                 │
│  │ CID 10    │  │ CID 11    │  │ CID 12      │                 │
│  │ Agent     │  │ Agent     │  │ Agent       │                 │
│  │ PipeWire  │  │ PipeWire  │  │ PipeWire    │                 │
│  └───────────┘  └───────────┘  └─────────────┘                 │
└──────────────────────────────────────────────────────────────────┘
          ▲                ▲                ▲
     Client A         Client B        Client C
     (JWT cid=10)     (JWT cid=11)    (JWT cid=12)
```

### Flux de session complet

```
1. Client → Broker (:7611)     auth mTLS, reçoit JWT + proxy_address
2. Broker → Host-agent         clone template → set_vsock_cid → start VM
3. Client → Proxy (:7610)      JWT vérifié, cid extrait
4. Agent (VM) → Proxy          vsock :6100, frames brutes
5. Proxy → Client              H.264 encodé, chiffré E2E
6. Client ferme                Proxy → Host-agent : stop + delete VM
```

---

## 2. Trait VmProvider

### Problème

Le broker était couplé à Proxmox via un module monolithique `proxmox/mod.rs` (521 lignes). Aucune abstraction pour supporter d'autres hyperviseurs.

### Solution

Trait générique avec 4 implémentations feature-gated :

```rust
// nidan-broker/src/provider/mod.rs

#[async_trait]
pub trait VmProvider: Send + Sync + 'static {
    fn backend_name(&self) -> &'static str;
    async fn list_vms(&self) -> Result<Vec<ProviderVm>>;
    async fn get_status(&self, provider_id: &str) -> Result<ProviderVm>;
    async fn clone_vm(&self, template: &str, name: &str) -> Result<ProviderVm>;
    async fn start_vm(&self, provider_id: &str) -> Result<()>;
    async fn stop_vm(&self, provider_id: &str) -> Result<()>;
    async fn delete_vm(&self, provider_id: &str) -> Result<()>;
    async fn set_vsock_cid(&self, provider_id: &str, cid: u32) -> Result<()>;
}
```

### Factory

```rust
pub fn build_provider(config: &ProviderConfig) -> Result<Arc<dyn VmProvider>> {
    match config.backend.as_str() {
        "static"      => Ok(Arc::new(StaticProvider)),
        "proxmox"     => { /* feature provider-proxmox */ }
        "libvirt"     => { /* feature provider-libvirt */ }
        "host-agent"  => { /* feature provider-host-agent */ }
        other         => bail!("backend inconnu: {other}"),
    }
}
```

### Compilation sélective

```toml
# nidan-broker/Cargo.toml
[features]
provider-proxmox    = ["dep:reqwest"]
provider-libvirt    = ["dep:virt"]
provider-host-agent = ["dep:tokio-vsock"]
```

```bash
cargo build -p nidan-broker                              # StaticProvider uniquement
cargo build -p nidan-broker --features provider-libvirt  # + LibvirtProvider (dev)
cargo build -p nidan-broker --features provider-host-agent # + HostAgentProvider (prod)
```

---

## 3. LibvirtProvider

### Rôle

Backend de développement — le broker appelle libvirt directement (nécessite un accès au socket Unix libvirtd). Non utilisé en production (remplacé par le host-agent).

### Opérations implémentées

| Opération | Appel libvirt | Sécurité |
|-----------|---------------|----------|
| `clone_vm` | `vol_create_xml_from` + `domain_define_xml` | Template arrêté, nom unique, préfixe forcé |
| `delete_vm` | `vol.delete` + `domain.undefine` | Suppression volumes AVANT undefine |
| `set_vsock_cid` | Modification XML `<vsock>` + `domain_define_xml` | Ajout/remplacement dans `<devices>` |

### Durcissement (Phase 4.0)

```rust
// Suppression des volumes AVANT undefine (évite les orphelins)
for path in extract_disk_paths(&xml) {
    if let Ok(vol) = StorageVol::lookup_by_path(&conn, &path) {
        vol.delete(0)?;
    }
}
dom.undefine()?;

// Validation clone : template arrêté + nom unique + préfixe forcé
if tmpl.is_active().unwrap_or(false) {
    bail!("template actif — arrêter avant de cloner");
}
let name = format!("{prefix}{}", &Uuid::new_v4().to_string()[..8]);
if Domain::lookup_by_name(&conn, &name).is_ok() {
    bail!("domaine '{name}' existe déjà");
}
```

---

## 4. Pool dynamique

### CidAllocator

Allocation de CIDs vsock sur une plage configurable :

```rust
// nidan-broker/src/pool/mod.rs

pub struct CidAllocator {
    next: u32,
    max: u32,
    free: Vec<u32>,
}

impl CidAllocator {
    pub fn allocate(&mut self) -> Option<u32> {
        if let Some(cid) = self.free.pop() {
            return Some(cid);       // Réutilise un CID libéré
        }
        if self.next <= self.max {
            let cid = self.next;
            self.next += 1;
            return Some(cid);       // Alloue le suivant
        }
        None                         // Plage épuisée
    }

    pub fn release(&mut self, cid: u32) {
        self.free.push(cid);         // Rend le CID disponible
    }
}
```

### assign_or_provision

```rust
// Flux d'assignation avec fallback dynamique

pub async fn assign_or_provision(&self, ...) -> Result<VmPoolEntry> {
    // 1. Chercher une VM statique disponible
    if let Some(vm) = self.find_available(preferred_tag) {
        vm.state = VmState::Assigned { session_id, since: Utc::now() };
        return Ok(vm);
    }

    // 2. Pool vide → provisionnement dynamique
    let cid = self.cid_allocator.lock()?.allocate()
        .ok_or_else(|| anyhow!("plage CID épuisée"))?;

    let vm = self.provider.clone_vm(&template, "").await?;
    self.provider.set_vsock_cid(&vm.provider_id, cid).await?;
    self.provider.start_vm(&vm.provider_id).await?;

    // 3. Enregistrer dans le pool
    let entry = VmPoolEntry {
        id: vm.name,
        dynamic: true,
        cid: Some(cid),
        provider_id: Some(vm.provider_id),
        state: VmState::Assigned { session_id, since: Utc::now() },
    };
    self.vms.insert(entry.id.clone(), entry.clone());
    Ok(entry)
}
```

### release() async

```rust
pub async fn release(&self, vm_id: &str, session_id: &str) {
    if is_dynamic {
        // VM dynamique : destruction complète
        self.provider.stop_vm(provider_id).await?;
        self.provider.delete_vm(provider_id).await?;
        self.cid_allocator.lock()?.release(cid);
        self.vms.remove(vm_id);
    } else {
        // VM statique : recyclage
        entry.state = VmState::Available;
    }
}
```

### GC des sessions expirées

```rust
// Tâche de fond (toutes les 30s)
pub async fn gc_expired_sessions(&self, max_age_secs: u64) {
    for entry in self.vms.iter() {
        if let VmState::Assigned { since, .. } = &entry.state {
            if (Utc::now() - *since).num_seconds() > max_age_secs as i64 {
                self.release(&entry.id, &session_id).await;
            }
        }
    }
}
```

### Configuration

```toml
# nidan-broker.toml
[pool.dynamic]
template = "nidan-template"
vm_port = 7610
cid_start = 10
cid_end = 99
```

---

## 5. Canal 2 — pilotage hyperviseur via vsock

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│  Socle KVM                                              │
│                                                         │
│  ┌───────────────────────────────────────────────────┐  │
│  │  nidan-host-agent (binaire Rust, service systemd) │  │
│  │                                                   │  │
│  │  Écoute : AF_VSOCK port 6900                      │  │
│  │  allowed_cid = 3 (broker, toutes ops)             │  │
│  │  proxy_cid = 1 (proxy, stop/delete)               │  │
│  │                                                   │  │
│  │  → libvirtd (socket Unix)                         │  │
│  └───────────────────┬───────────────────────────────┘  │
│                      │ vsock                             │
│  ┌───────────────────┴───────────────────────────────┐  │
│  │  VM Broker (CID 3)                                │  │
│  │  HostAgentProvider (impl VmProvider)               │  │
│  │  → connect vsock CID 2:6900                       │  │
│  │  → envoie AgentRequest (JSON, length-prefix)      │  │
│  │  → reçoit AgentResponse                           │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

### Protocole (nidan-proto/src/host_agent.rs)

```rust
// Requêtes (broker ou proxy → agent)
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AgentRequest {
    ListVms { prefix: String },
    GetStatus { vm_id: String },
    CloneVm { template: String, new_name: String },
    StartVm { vm_id: String },
    StopVm { vm_id: String },
    DeleteVm { vm_id: String },
    SetVsockCid { vm_id: String, cid: u32 },
}

// Réponses
pub struct AgentResponse {
    pub success: bool,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}
```

### Transport

```
[4 bytes BE longueur][JSON payload]

Exemple :
  [00 00 00 2A]{"action":"start_vm","vm_id":"uuid-123"}
```

### HostAgentProvider (client vsock dans le broker)

```rust
// nidan-broker/src/provider/host_agent.rs

async fn send_request(&self, req: &AgentRequest) -> Result<AgentResponse> {
    let addr = VsockAddr::new(self.config.host_cid, self.config.port);
    let mut stream = VsockStream::connect(addr).await?;

    // Envoyer
    let payload = serde_json::to_vec(req)?;
    stream.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    stream.write_all(&payload).await?;

    // Recevoir
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let mut buf = vec![0u8; u32::from_be_bytes(len_buf) as usize];
    stream.read_exact(&mut buf).await?;

    serde_json::from_slice(&buf)
}
```

### Nouveau crate : nidan-host-agent

```
nidan-host-agent/
├── Cargo.toml           (virt, tokio-vsock, nidan-proto)
└── src/
    ├── main.rs           écoute vsock, accept, CID check, framing
    ├── config.rs         VsockConfig + LibvirtConfig + SecurityConfig
    ├── handler.rs        dispatch + validation préfixe + restriction proxy
    └── libvirt_ops.rs    7 opérations + lookup_and_verify + helpers XML
```

---

## 6. JWT CID et routage proxy

### Problème

Le broker retournait `vm.addr()` (IP directe de la VM) au client. Le client devait connaître l'adresse de la VM.

### Solution

Le JWT embarque le CID vsock et le broker retourne l'adresse du proxy-encoder :

```rust
// nidan-broker/src/auth/jwt.rs
pub struct SessionClaims {
    pub sub: String,
    pub session_id: String,
    pub vm_id: String,
    pub cid: Option<u32>,          // CID vsock de la VM
    // ...
}

// nidan-broker/src/routing/mod.rs
server_address: state.config.network.proxy_address
    .clone()
    .unwrap_or_else(|| vm.addr()),  // fallback si pas de proxy
```

### JWT signé

```json
{
  "sub": "nidan-client",
  "session_id": "4ed7c28e-...",
  "vm_id": "nidan-a4fa9e25",
  "cid": 10,
  "iss": "nidan-broker",
  "exp": 1724972400
}
```

Le proxy vérifie la signature HMAC-SHA256 (clé partagée broker↔proxy) et extrait le CID pour le routage.

---

## 7. Multi-VM — routage frames et inputs par CID

### Routage des frames

```rust
// nidan-proxy-encoder/src/capture/mod.rs
pub struct RawFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    // ...
    pub source_cid: Option<u32>,    // None=local, Some(cid)=vsock agent
}

// nidan-proxy-encoder/src/capture/vsock.rs
// Chaque frame taguée avec le CID de l'agent source
let raw = RawFrame {
    data: proto_frame.pixels,
    source_cid: Some(peer_cid),     // ← tag CID
    // ...
};
```

```rust
// nidan-proxy-encoder/src/capture/vsock_service.rs
// Filtrage par CID dans l'abonnement
pub fn subscribe_frames_as_mpsc(
    &self,
    shutdown: CancellationToken,
    filter_cid: Option<u32>,        // ← filtre par CID
) -> mpsc::Receiver<RawFrame> {
    // ...
    if let Some(target_cid) = filter_cid {
        if f.source_cid != Some(target_cid) {
            continue;               // Frame d'un autre agent → skip
        }
    }
}
```

### Multi-agent simultané

```rust
// nidan-proxy-encoder/src/capture/vsock.rs — accept loop
// AVANT : run_session().await (bloquant, mono-agent)
// APRÈS : tokio::spawn(run_session()) (concurrent, multi-agent)

tokio::spawn(async move {
    let result = run_session(
        stream, session_tx, session_inputs,
        session_fps, session_sd, session_caps,
        session_notify, peer_cid,
    ).await;
});
```

### Routage des inputs par CID

```
PROBLÈME :
  Client A (cid=10) → inputs → canal partagé → Agent CID 10 (mutex) ← reçoit TOUT
  Client B (cid=11) → inputs → canal partagé → RIEN (mutex pris)

SOLUTION :
  Client A → register_input_for_cid(10) → tx_10
  Client B → register_input_for_cid(11) → tx_11
  Agent CID 10 connecte → prend rx_10 → reçoit inputs de A ✅
  Agent CID 11 connecte → prend rx_11 → reçoit inputs de B ✅
```

```rust
// Pré-enregistrement AVANT wait_for_agent (timing critique)
// nidan-proxy-encoder/src/stream/mod.rs — handle_connection()

if let Some(cid) = session_cid {
    service.register_input_for_cid(cid);  // ← AVANT le wait
}
let real_caps = service.wait_for_agent_capabilities(...).await?;
//                                                       ↑ l'agent se connecte ICI
//                                                         et trouve le rx pour son CID
```

```rust
// nidan-proxy-encoder/src/capture/vsock.rs — à la connexion de l'agent
let session_inputs = {
    let mut rxs = self.cid_input_rxs.lock().unwrap();
    if let Some(rx) = rxs.remove(&peer_cid) {
        // Canal CID dédié trouvé
        Arc::new(Mutex::new(rx))
    } else {
        // Fallback : canal partagé (mono-VM compat)
        inputs_rx
    }
};
```

```rust
// make_injector récupère le tx par lookup
tx: session_cid
    .and_then(|cid| service.get_input_tx_for_cid(cid))
    .unwrap_or_else(|| service.inputs_tx()),
```

### Schéma complet multi-VM

```
Client A (JWT cid=10)                      Client B (JWT cid=11)
    │                                          │
    │ QUIC/mTLS + JWT                          │ QUIC/mTLS + JWT
    ▼                                          ▼
┌──────────────────────────────────────────────────────────┐
│  Proxy-Encoder                                           │
│                                                          │
│  Session A :                                             │
│    register_input_for_cid(10) → tx_A                     │
│    subscribe_frames(filter_cid=10) → frames CID 10       │
│    make_injector → get_input_tx(10) → tx_A               │
│                                                          │
│  Session B :                                             │
│    register_input_for_cid(11) → tx_B                     │
│    subscribe_frames(filter_cid=11) → frames CID 11       │
│    make_injector → get_input_tx(11) → tx_B               │
│                                                          │
│  VsockCapturer (port 6100) :                             │
│    Agent CID 10 → rx_A → inputs session A                │
│    Agent CID 11 → rx_B → inputs session B                │
│    Frames CID 10 → broadcast → filtre → session A        │
│    Frames CID 11 → broadcast → filtre → session B        │
└──────────────────────────────────────────────────────────┘
```

---

## 8. Destruction immédiate à la déconnexion

### Problème initial

Le `release()` était appelé quand le client se déconnectait du **broker** (après le handshake). Mais la VM devait rester active pour la session proxy :

```
Client → Broker (handshake) → JWT → Client se déconnecte du broker
                                      → release() → VM détruite ← BUG
Client → Proxy → VM n'existe plus ← ÉCHEC
```

### Solution en deux parties

**Partie 1 — Dissocier le release du broker :**

Le broker ne release plus à la déconnexion. Un GC périodique (30s) détruit les VMs dont le JWT a expiré (fallback de sécurité).

**Partie 2 — Destruction immédiate par le proxy :**

À la déconnexion du client, le proxy envoie `stop_vm` + `delete_vm` au host-agent via vsock loopback :

```rust
// nidan-proxy-encoder/src/stream/mod.rs

#[cfg(feature = "vsock-source")]
async fn cleanup_dynamic_vm(vm_id: &str) {
    use nidan_proto::host_agent::{AgentRequest, HOST_AGENT_DEFAULT_PORT};

    let addr = tokio_vsock::VsockAddr::new(1, HOST_AGENT_DEFAULT_PORT);
    //                                     ↑ CID 1 = loopback vsock

    for req in [
        AgentRequest::StopVm { vm_id: vm_id.to_string() },
        AgentRequest::DeleteVm { vm_id: vm_id.to_string() },
    ] {
        // Envoie via vsock, best-effort
        let mut stream = VsockStream::connect(addr).await?;
        // ... framing length-prefix JSON ...
    }
}
```

### Restriction du proxy dans le host-agent

```rust
// nidan-host-agent/src/handler.rs

let is_proxy = cfg.vsock.proxy_cid.map_or(false, |c| peer_cid == c);
if is_proxy {
    match &req {
        AgentRequest::StopVm { .. } | AgentRequest::DeleteVm { .. } => {}
        _ => {
            return AgentResponse::err(
                "opération non autorisée pour le proxy (stop_vm/delete_vm uniquement)"
            );
        }
    }
}
```

### Flux corrigé

```
Client → Broker     handshake → JWT → VM reste Assigned (pas de release)
Client → Proxy      session active (durée du JWT)
Client ferme        → Proxy détecte conn.closed()
                    → cleanup_dynamic_vm(vm_id)
                       → vsock CID 1:6900 → host-agent
                       → stop_vm OK
                       → delete_vm OK
                    → VM détruite immédiatement

GC (fallback)       toutes les 30s, libère les VMs dont le JWT a expiré
                    (si le cleanup du proxy a échoué)
```

---

## 9. Thin clone — backing file qcow2

### Problème

Le clone copie les 16 Go du disque template sur un HDD SATA (~50 secondes). Avec 3 VMs simultanées, le HDD sature et les VMs passent en pause I/O.

### Solution

Thin clone via backing file — instantané, quelques Ko au lieu de 16 Go :

```rust
// nidan-host-agent/src/libvirt_ops.rs

// AVANT : copie complète (StorageVol::create_xml_from)
StorageVol::create_xml_from(&pool, &vol_xml, &src_vol, 0)?;

// APRÈS : backing file (qemu-img)
std::process::Command::new("qemu-img")
    .args(["create", "-f", "qcow2",
           "-b", &src_path,        // image de base (template)
           "-F", "qcow2",          // format de la base
           &new_path])             // image overlay (diff uniquement)
    .output()?;
pool.refresh(0)?;                  // libvirt voit le nouveau volume
```

### Résultat

| Métrique | Clone complet | Thin clone |
|----------|---------------|------------|
| Temps | ~50 secondes | < 1 seconde |
| Espace disque | 16 Go par clone | Quelques Ko (diff) |
| I/O disque | Saturation HDD | Négligeable |
| VMs simultanées | 2 max (puis pause I/O) | 10+ sans problème |

---

## 10. Analyse de sécurité

### Périmètre de confiance

```
┌────────────────────────────────────┐
│  RÉSEAU (non fiable)               │
│  • Client NIDAN                    │
│  • Internet                        │
└──────────────┬─────────────────────┘
               │
    ═══════════╪═══════════════════ FRONTIÈRE (QUIC/mTLS)
               │
┌──────────────┴─────────────────────┐
│  SOCLE (fiable)                    │
│  • Proxy-Encoder                   │
│  • Host-Agent                      │
│  • libvirtd                        │
│  • VM Broker (vsock, pas réseau)   │
│  • VMs Desktop (vsock, pas réseau) │
└────────────────────────────────────┘
```

### Sécurité du canal 2 (vsock)

| Propriété | Mécanisme |
|-----------|-----------|
| **Pas de réseau IP** | vsock = bus virtio, invisible aux outils réseau |
| **CID non falsifiable** | Assigné par l'hyperviseur, pas configurable depuis la VM |
| **Filtrage CID** | Host-agent vérifie `allowed_cid` (broker) et `proxy_cid` (proxy) |
| **Allow-list d'opérations** | Proxy limité à `stop_vm` + `delete_vm` |
| **Préfixe forcé** | Toute opération vérifiée contre `nidan-*` (handler + lookup_and_verify) |
| **Pas de XML brut** | Requêtes typées JSON, l'agent construit le XML |
| **Audit** | Chaque requête loggée avec timestamp, CID source, action, résultat |

### Double vérification du préfixe

```
Requête : { "action": "delete_vm", "vm_id": "uuid-dune-vm-systeme" }

Niveau 1 (handler.rs) :
  validate_prefix() → CloneVm/ListVms vérifient le préfixe
  (pour delete_vm, le vm_id est un UUID → vérification reportée)

Niveau 2 (libvirt_ops.rs) :
  lookup_and_verify() :
    UUID → résolution → nom = "debian-base"
    "debian-base".starts_with("nidan-") → false → REJET
```

### Ce que le host-agent empêche (vs accès libvirt direct)

| Attaque | Libvirt direct | Via host-agent |
|---------|---------------|----------------|
| `define_xml` avec `<hostdev>` (PCI passthrough) | ✅ possible | ❌ bloqué |
| `define_xml` avec `<filesystem>` (montage hôte) | ✅ possible | ❌ bloqué |
| `vol-download` (exfiltration images disque) | ✅ possible | ❌ bloqué |
| Opérer sur des VMs hors périmètre NIDAN | ✅ possible | ❌ bloqué |
| Clone en boucle (DoS ressources) | ✅ possible | ⚠️ possible (nidan-* uniquement) |

### Sécurité du JWT

| Propriété | Mécanisme |
|-----------|-----------|
| **Authenticité** | Signature HMAC-SHA256, clé partagée broker↔proxy |
| **Intégrité** | Toute modification invalide la signature |
| **Anti-replay** | Session ID unique + expiration |
| **Scope** | Le CID dans le JWT donne accès à UNE VM spécifique |
| **Confidentialité** | CID visible en Base64 mais inexploitable hors du socle (vsock non routable) |

### Chiffrement E2E

```
Client ←→ Proxy :
  X25519 (échange de clés éphémères)
  + ChaCha20-Poly1305 (chiffrement symétrique)
  → flux vidéo + inputs chiffrés de bout en bout
  → le réseau ne voit que du bruit
```

---

## 11. Tests end-to-end validés

### Environnement de test

| Composant | Détails |
|-----------|---------|
| Socle | Dell R440, 32 vCPUs, 157 Go RAM, HDD SATA 1.1 To |
| OS socle | Ubuntu (kernel avec vsock + vhost_vsock + vsock_loopback) |
| VM Broker | CID 3, vsock vers socle |
| Template | Ubuntu 22.10, 20 Go RAM, 10 vCPUs, nidan-agent PipeWire |
| Clients | 2 machines distinctes |

### Tests validés

| Test | Résultat |
|------|----------|
| Broker démarre (provider=host-agent) | ✅ |
| Host-agent connecte libvirt + écoute vsock | ✅ |
| Canal 2 : broker → agent list_vms | ✅ |
| Canal 2 : clone + set_cid + start | ✅ |
| Client 1 : auth → VM dynamique → flux H.264 | ✅ |
| Client 2 : auth → VM dynamique → flux H.264 | ✅ |
| Client 3 : auth → VM dynamique → flux H.264 | ✅ (après thin clone) |
| Routage frames : chaque client voit SA VM | ✅ |
| Routage inputs : chaque client contrôle SA VM | ✅ |
| Déconnexion : VM détruite immédiatement | ✅ |
| Sécurité : proxy limité à stop/delete | ✅ |
| E2E crypto (X25519 + ChaCha20-Poly1305) | ✅ |
| Presse-papier bidirectionnel | ✅ |

### Logs de validation (extrait)

```
# Host-agent — cycle complet
clone_vm OK name=nidan-dbcf8255
set_vsock_cid OK cid=11
start_vm OK

# Proxy — routage CID
canal inputs dédié enregistré cid=11       ← avant wait_for_agent
agent connecté peer_cid=11
inputs routés via canal CID dédié          ← agent trouve le canal

# Proxy — cleanup à la déconnexion
peer_cid=1 action=stop_vm                 ← proxy (CID loopback)
stop_vm OK
peer_cid=1 action=delete_vm
delete_vm OK
```

---

## 12. Points restants à traiter

### Priorité haute

| Sujet | Description | Effort |
|-------|-------------|--------|
| **Échelon E — Pool chaud** | Pré-provisionner `min_available` VMs au démarrage. Réduit le temps d'attente du premier client (clone à froid = ~1s thin, + ~40s boot VM) | 1 jour |
| **Échelons F-G — GC + quotas** | Nettoyage des VMs orphelines au démarrage, quota max par utilisateur, limite de clones simultanés | 1-2 jours |
| **Systemd units** | Services systemd pour host-agent et proxy (restart, logging, dépendances) | 2h |
| **Release v0.8.0** | Commit, tag, push, release notes GitHub | 1h |

### Priorité moyenne

| Sujet | Description | Effort |
|-------|-------------|--------|
| **Optimisation boot VM** | Cloud-init minimal, réduire les services au boot pour passer de 40s à ~15s | 1 jour |
| **Réduction RAM template** | Passer de 20 Go à 4 Go par VM (suffisant pour un bureau avec agent PipeWire) | 15 min |
| **Backing file consolidation** | Périodiquement merger les overlays dans le backing file pour éviter les chaînes trop longues | 1 jour |
| **Input routing nettoyage** | Supprimer les entrées `cid_input_txs`/`cid_input_rxs` quand l'agent se déconnecte (fuite mémoire légère) | 2h |
| **Métriques Prometheus** | Ajouter des métriques pour le pool dynamique (VMs actives, clones en cours, temps de provisionnement) | 1 jour |

### Priorité basse (thèse / innovation C2-E)

| Sujet | Description | Effort |
|-------|-------------|--------|
| **Firewall sémantique XML** | L'agent parse et valide le XML libvirt qu'il génère (whitelist devices, chemins, plages CID). Aucun outil existant ne fait ça (polkit filtre par verbe, pas par contenu) | 2-4 semaines |
| **Capacités CID-bound** | Jetons cryptographiques liés au CID vsock du broker, scopés par opération et périmètre, temporaires. Combine les systèmes de capacités (CHERI, seL4) avec l'identité vsock | 2-4 semaines |
| **Attestation mutuelle IMA/EVM** | Au boot, broker et agent s'échangent leurs mesures d'intégrité via vsock. L'attestation existante (SEV-SNP/TDX) protège la VM contre l'hôte ; ici c'est bidirectionnel sans matériel spécifique | 2-4 semaines |

### Bugs connus

| Bug | Impact | Contournement |
|-----|--------|---------------|
| **Quinn UDP sendmsg error** | Warning côté client au début de chaque connexion. N'affecte pas le fonctionnement | Ignoré (log level) |
| **Clone lent sur HDD** | ~1s (thin clone) + ~40s (boot VM). Acceptable pour un POC, pas pour la production | Pool chaud (échelon E) |
| **libvirt "Invalid UUID" stderr** | Messages libvirt sur stderr lors du `lookup_by_name` initial. Pas une erreur, c'est la vérification "le domaine n'existe pas encore" | Cosmétique, ignoré |

---

## Annexe — Fichiers modifiés

### Nouveaux fichiers

| Fichier | Lignes | Rôle |
|---------|--------|------|
| `nidan-host-agent/Cargo.toml` | ~25 | Manifeste du crate |
| `nidan-host-agent/src/main.rs` | ~130 | Listener vsock + accept loop |
| `nidan-host-agent/src/config.rs` | ~80 | Configuration TOML |
| `nidan-host-agent/src/handler.rs` | ~100 | Dispatch + validation + restriction proxy |
| `nidan-host-agent/src/libvirt_ops.rs` | ~280 | 7 opérations libvirt |
| `nidan-proto/src/host_agent.rs` | ~200 | Types protocole + 10 tests |
| `nidan-broker/src/provider/mod.rs` | ~180 | Trait VmProvider + factory |
| `nidan-broker/src/provider/libvirt.rs` | ~520 | LibvirtProvider |
| `nidan-broker/src/provider/host_agent.rs` | ~160 | HostAgentProvider (vsock client) |

### Fichiers modifiés

| Fichier | Changements |
|---------|-------------|
| `Cargo.toml` (workspace) | +1 member |
| `nidan-broker/Cargo.toml` | +3 dépendances, +2 features |
| `nidan-broker/src/config.rs` | +150 lignes (ProviderConfig, DynamicPoolConfig, HostAgentProviderConfig) |
| `nidan-broker/src/pool/mod.rs` | +300 lignes (CidAllocator, assign_or_provision, release async, GC) |
| `nidan-broker/src/routing/mod.rs` | +40 lignes (build_provider, GC task, proxy_address) |
| `nidan-broker/src/auth/jwt.rs` | +15 lignes (cid dans SessionClaims) |
| `nidan-proto/src/lib.rs` | +8 lignes (pub mod host_agent) |
| `nidan-proxy-encoder/src/capture/mod.rs` | +5 lignes (source_cid) |
| `nidan-proxy-encoder/src/capture/vsock.rs` | +80 lignes (multi-agent, CID tag, per-CID inputs) |
| `nidan-proxy-encoder/src/capture/vsock_service.rs` | +40 lignes (filter_cid, register/get CID inputs) |
| `nidan-proxy-encoder/src/session_token.rs` | +3 lignes (cid dans SessionClaims) |
| `nidan-proxy-encoder/src/stream/mod.rs` | +90 lignes (CID routing, cleanup VM, session_vm_id) |

---

*Document de référence pour la release v0.8.0-dynamic-pool. Complète les synthèses détaillées C2-A, C2-B, C2-C et les release notes.*
