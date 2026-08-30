# NIDAN v2 — v0.8.0-dynamic-pool

## Release Notes

> **Date :** 30 août 2026
> **Branche :** main
> **Précédent tag :** v0.7.3-client-perf
> **Testé :** 3 clients simultanés, 3 VMs dynamiques, vidéo + clavier + souris isolés

---

### Résumé

Cette release introduit le **pool dynamique de VMs**, le **canal de pilotage vsock** (Canal 2), le **routage multi-VM** (frames et inputs par CID), la **destruction immédiate** des VMs à la déconnexion client, et le **thin clone** (backing file qcow2 instantané). Le broker provisionne, démarre et détruit des VMs à la demande via un agent dédié sur le socle, sans accès libvirt direct.

---

### Nouveau crate

**`nidan-host-agent`** — agent de pilotage libvirt sur le socle KVM, communiquant via vsock.

- Écoute `AF_VSOCK` port 6900
- Filtrage CID source : broker (CID 3, toutes ops) / proxy (CID 1, stop+delete uniquement)
- 7 opérations : `list_vms`, `get_status`, `clone_vm`, `start_vm`, `stop_vm`, `delete_vm`, `set_vsock_cid`
- Validation préfixe `nidan-*` sur chaque opération (handler + lookup_and_verify)
- Pas de XML libvirt brut exposé — l'agent construit le XML côté socle

---

### nidan-broker

#### Trait VmProvider + 4 backends

| Backend | Feature | Transport | Usage |
|---------|---------|-----------|-------|
| `static` | (défaut) | — | VMs déclarées en config |
| `libvirt` | `provider-libvirt` | socket Unix libvirt | Développement local |
| `proxmox` | `provider-proxmox` | HTTPS REST | Legacy (stub) |
| `host-agent` | `provider-host-agent` | vsock | **Production** |

#### Pool dynamique

- **`CidAllocator`** — allocation de CIDs vsock sur plage configurable (`cid_start`..`cid_end`)
- **`assign_or_provision()`** — assignation statique puis clone automatique si pool vide
- **`release()` async** — VMs statiques recyclées, VMs dynamiques détruites (stop → delete → libération CID)
- **GC sessions expirées** — tâche de fond (30s), libère les VMs dont le JWT a expiré (filet de sécurité)

#### JWT CID + proxy_address

- Le JWT contient `cid: Option<u32>` — le CID vsock de la VM assignée
- `server_address` retourne `proxy_address` (proxy-encoder) au lieu de l'IP de la VM
- Fallback `vm.addr()` si `proxy_address` non configuré (rétrocompatibilité)

#### Configuration

```toml
[provider]
backend = "host-agent"

[provider.host_agent]
host_cid = 2
port = 6900
vm_prefix = "nidan-"

[pool.dynamic]
template = "nidan-template"
vm_port = 7610
cid_start = 10
cid_end = 99

[network]
proxy_address = "192.168.8.199:7610"
```

---

### nidan-proxy-encoder

#### Routage multi-VM par CID

- **`RawFrame.source_cid`** — chaque frame taguée avec le CID de l'agent source
- **`VsockCapturer` multi-agent** — `tokio::spawn` par connexion agent
- **`subscribe_frames_as_mpsc(shutdown, filter_cid)`** — chaque session client ne reçoit que les frames de sa VM
- **Routage inputs per-CID** — canaux dédiés `register_input_for_cid` + `get_input_tx_for_cid`, pré-enregistrement avant `wait_for_agent` (fix timing deux wait)
- **`SessionClaims.cid`** — le proxy lit le CID du JWT

#### Destruction immédiate à la déconnexion

- À `conn.closed()`, le proxy envoie `stop_vm` + `delete_vm` au host-agent via **vsock loopback** (CID 1, module `vsock_loopback`)
- Best-effort : erreurs loggées, pas de blocage
- Vérification : seules les VMs dynamiques (`nidan-*`, ≠ template) sont détruites
- Le host-agent restreint le proxy à stop/delete — toute autre opération est rejetée

#### Rétrocompatibilité

- `filter_cid = None` → toutes les frames (mode mono-VM inchangé)
- Captureurs locaux (X11, PipeWire, Stub) → `source_cid: None`

---

### nidan-proto

#### Module `host_agent`

- `AgentRequest` — enum 7 variants, sérialisation JSON taguée
- `AgentResponse` — `success` + `result` + `error`, helpers (`ok_empty`, `ok_with`, `err`, `into_result`)
- `AgentVm` — `provider_id`, `name`, `status`
- Constantes : `HOST_AGENT_DEFAULT_PORT = 6900`, `VMADDR_CID_HOST = 2`
- 10 tests unitaires

---

### Thin clone qcow2

- `clone_volumes` utilise `qemu-img create -b` (backing file) au lieu de `StorageVol::create_xml_from` (copie complète)
- Clone **instantané** (~0s) au lieu de ~50s pour 16 Go sur HDD
- Résout la saturation I/O qui mettait les VMs en pause avec 3+ clones simultanés

---

### LibvirtProvider (provider-libvirt)

Implémentation complète pour développement/test :

- Clone volumes + rewrite XML + validation (template arrêté, nom unique, préfixe forcé)
- Delete volumes avant undefine (pas d'orphelins)
- Set vsock CID dans le XML du domaine
- 5 tests unitaires XML

---

### Sécurité

| Couche | Mécanisme |
|--------|-----------|
| Réseau → Socle | QUIC/mTLS (frontière de sécurité) |
| Broker → Host-agent | vsock CID 3, 7 opérations, préfixe `nidan-*` |
| Proxy → Host-agent | vsock CID 1 (loopback), **stop/delete uniquement** |
| Host-agent → libvirt | Double vérification préfixe (handler + lookup_and_verify) |
| Broker → Client | JWT signé HMAC-SHA256, expiration courte |
| Proxy → Client | E2E crypto (X25519 + ChaCha20-Poly1305) |
| Isolation inter-sessions | Routage frames + inputs par CID source |

---

### Tests end-to-end validés

| Test | Résultat |
|------|----------|
| 3 clients simultanés, 3 VMs dynamiques | ✅ |
| Routage frames par CID (isolation vidéo) | ✅ |
| Routage inputs par CID (isolation clavier/souris) | ✅ |
| Thin clone instantané sur HDD | ✅ |
| Destruction VM immédiate à la déconnexion | ✅ |
| E2E crypto + presse-papier bidirectionnel | ✅ |
| Proxy restreint à stop/delete par le host-agent | ✅ |

---

### Breaking changes

- `subscribe_frames_as_mpsc()` prend `filter_cid: Option<u32>` (ajouter `None` pour l'ancien comportement)
- `issue_session_token()` / `jwt.sign()` prennent `cid: Option<u32>`
- `RawFrame` a un champ `source_cid: Option<u32>`
- `handle_request()` du host-agent prend `peer_cid: u32`

---

### Prochaines étapes

- **Échelons E-G** — pool chaud, GC orphelines au boot, quotas par utilisateur
- **Optimisation** — réduction RAM template (20→4 Go), boot VM rapide (~15s)
- **Systemd units** — host-agent + proxy services
- **C2-E** — brique innovation thèse IDPE : firewall sémantique XML, capacités CID-bound, attestation mutuelle IMA/EVM
