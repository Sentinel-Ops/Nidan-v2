# NIDAN v2 — Plan d'action : migration Proxmox → KVM/libvirt pur

**Date** : 24 août 2026
**Contexte** : abandon de Proxmox au profit d'une stack KVM/libvirt pure
(hôte Dell R440). Reprise du développement du pool dynamique là où la
session du 16-17 juillet 2026 l'a laissé (échelons A+B Proxmox terminés,
échelons C-G à faire).

---

## Décision d'architecture

**Ni nouveau repo, ni fork. Même repo `Sentinel-Ops/Nidan-v2`, avec :**

1. **Sanctuarisation** de la version Proxmox fonctionnelle par tags Git
2. **Abstraction** `VmProvider` (trait Rust) découplant le broker de
   l'hyperviseur
3. Code Proxmox conservé derrière un **feature flag Cargo**
   (`proxmox-provider`) — non compilé par défaut, hors périmètre CSPN,
   réactivable si besoin commercial
4. Nouvelle implémentation **`LibvirtProvider`** (socket Unix local
   `qemu:///system`, crate `virt`)

### Justification

- Un seul module est couplé à Proxmox : `nidan-broker/src/proxmox/mod.rs`.
  Tout le reste (agent, client, proxy-encoder, proto, routing, mTLS,
  QUIC, VmPool) est agnostique. Le proxy écoute déjà sur
  `VMADDR_CID_ANY`.
- Un repo unique préserve l'historique traçable exigé pour la CSPN.
- Les corrections transverses (ex. fix portail) ne sont portées qu'une
  fois.

### Gain majeur au passage à libvirt

La contrainte **« une seule VM dynamique active à la fois »** (session
16-17 juillet) venait de la limitation Proxmox : `args` réservé root →
CID vsock non modifiable par API. **Avec libvirt, le CID est un élément
du XML de domaine entièrement contrôlé par le broker** :

```
VM 201 → CID 201, VM 202 → CID 202, ...
```

Le multi-session dynamique sur un seul hôte redevient possible. Le
tableau des alternatives écartées (hook scripts, SSH+sudoers, russh)
est obsolète.

### Correspondance API Proxmox → libvirt

| API actuelle (`ProxmoxClient`) | Équivalent libvirt | Remarque |
|---|---|---|
| `clone_vm(template, id, name)` | `qemu-img create -b` + `virDomainDefineXML` | Clone par backing file : quasi instantané (vs ~2 min) |
| `start_vm(id)` | `virDomainCreate` | Synchrone |
| `stop_vm(id)` | `virDomainShutdown` (+ timeout → `virDomainDestroy`) | |
| `delete_vm(id)` | `virDomainUndefine` + suppression qcow2 | |
| `get_vm_status(id)` | `virDomainGetState` | |
| `list_vms()` | `virConnectListAllDomains` | |
| `wait_for_task(upid, …)` | **supprimé** | libvirt est synchrone — tout le polling UPID disparaît |
| Token API + épinglage TLS SHA-256 | Socket Unix local | Plus d'API HTTP à sécuriser (broker et libvirtd co-localisés) |

---

## Les 5 phases

### Phase 0 — Sanctuarisation Git (30 min)

- Committer l'état actuel (étape 6i multisession-inputs) et taguer
  `v0.7.1-etape6i`
- Finaliser l'échelon B sur `feat-pool-dynamique-echelon-A`
  (supprimer le test temporaire, committer), merger dans `main`
- Taguer `v0.7.1-proxmox-final` : **dernière version supportant
  Proxmox** — point de restauration permanent

### Phase 1 — v0.7.2 : fix session portail unique (1-2h)

**À faire AVANT la divergence libvirt** : c'est du code agent,
indépendant de l'hyperviseur — le fix doit bénéficier aux deux mondes
et rester dans le tronc commun.

- Nouveau module `nidan-agent/src/portal_session.rs` : négociation
  portail XDG unique (RemoteDesktop + ScreenCast couplés sur UNE
  session D-Bus)
- Corrige le bug `notify_pointer_motion_absolute` (deux sessions
  séparées → stream_node invalide → curseur figé, clics au mauvais
  endroit)
- Bonus : une seule popup d'autorisation GNOME, un seul token de
  restauration, surface D-Bus réduite (argument CSPN)
- Inclure aussi le **clamp openh264** (dimensions paires) dans
  `nidan-proxy-encoder` — bug révélé par virtio-gpu, à corriger
  défensivement quel que soit le driver graphique
- Tag `v0.7.2-etape6j-portal-unifie`

### Phase 2 — Trait `VmProvider` (0.5 jour)

Branche `feat-libvirt-provider` depuis `main`.

- Extraire le trait depuis la signature de fait de `ProxmoxClient`
  (7 méthodes)
- Déplacer le code Proxmox derrière `#[cfg(feature = "proxmox-provider")]`
- Le broker consomme `Box<dyn VmProvider>` — sélection par config TOML

### Phase 3 — `LibvirtProvider` : équivalent échelons A+B (1 jour)

- Implémentation avec le crate `virt` sur `qemu:///system`
- Clone par backing file qcow2 + génération XML de domaine avec CID
  vsock unique par VM
- Template : recréer l'équivalent de la VM template 116 côté libvirt
  (qcow2 de base + XML template avec placeholder CID ; les tokens de
  portail persistés dans l'image restent valables)
- Validation : cycle complet clone → start → status → stop → delete,
  comme la validation Proxmox du 16-17 juillet (133 s ; attendu bien
  plus rapide avec backing files)

### Phase 4 — Échelons C-G sur le trait (planning inchangé)

Reprise exacte de la roadmap de la session du 16-17 juillet, écrite
contre le trait (donc valable pour tout provider futur) :

- **C** — Allocation VMID (plage 200-299) + intégration
  `VmPool::assign()` : provisionnement auto quand aucune VM statique
  disponible
- **D** — Destruction automatique post-session dans `VmPool::release()`
- **E** — Pool chaud (`min_available` de `PoolConfig`, déjà présent,
  inutilisé) — **désormais multi-VM possible grâce aux CIDs
  dynamiques**
- **F-G** — GC des VMs orphelines, quotas, nettoyage au démarrage du
  broker

---

## Arbre Git cible

```
main ─┬─ v0.7-etape6-stable-production        (tag existant)
      ├─ v0.7.1-etape6i                        (Phase 0)
      ├─ [merge feat-pool-dynamique-echelon-A]
      ├─ v0.7.1-proxmox-final                  (Phase 0 — point de restauration Proxmox)
      ├─ [merge fix/v0.7.2-portal-session-unique]
      ├─ v0.7.2-etape6j-portal-unifie          (Phase 1)
      └─ feat-libvirt-provider                 (Phases 2-3)
            └─ [échelons C-G]                  (Phase 4)
```

## Périmètre CSPN après migration

- **Dans le TOE** : agent (session portail unique), proxy-encoder,
  broker + `LibvirtProvider` (socket Unix local, surface minimale),
  client
- **Hors TOE** : code Proxmox (feature non compilée)
- L'historique Git montre une évolution maîtrisée : tags de
  sanctuarisation, branches thématiques, pas de fork sauvage
