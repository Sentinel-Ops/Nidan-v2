# Synthèse — Session du 24-27 août 2026
## Étape 7 : Migration Proxmox → KVM/libvirt — Phases 0 et 1

---

## Contexte de départ

NIDAN v2 était fonctionnel sur Proxmox (tag `v0.7.1-etape6i-multisession-inputs`),
avec les échelons A+B du pool dynamique terminés (session du 16-17 juillet 2026).
L'infrastructure de développement a basculé d'une stack Proxmox vers du
KVM/libvirt pur sur un serveur Dell R440.

**Trois objectifs pour cette session :**

1. Sanctuariser la version Proxmox fonctionnelle et documenter la
   décision d'architecture (Phase 0)
2. Corriger le bug d'injection d'inputs (curseur figé) découvert lors de
   la migration de la VM cible vers KVM/libvirt (Phase 1)
3. Préparer le terrain pour la reprise du pool dynamique sur libvirt
   (Phases 2-4)

---

## Ce qui a été fait

### Phase 0 — Sanctuarisation Git (24 août 2026)

**Objectif :** figer la version Proxmox fonctionnelle avant de diverger.

- Finalisation de l'échelon B du pool dynamique Proxmox : commit des
  5 méthodes d'écriture (`clone_vm`, `set_config`, `start_vm`,
  `stop_vm`, `delete_vm`, `wait_for_task`) sur la branche
  `feat-pool-dynamique-echelon-A`, test d'intégration retiré
  (secrets en dur nettoyés, token API Proxmox révoqué)
- Merge dans `main` + tag `v0.7.1-proxmox-final` : point de
  restauration permanent de la version Proxmox fonctionnelle
- Document `docs/plan-action-migration-kvm.md` : décision d'architecture
  tracée (trait `VmProvider`, feature flag `proxmox-provider`,
  `LibvirtProvider` comme cible primaire, ni fork ni nouveau repo)
- Mise à jour de `plan-dev-v2.md` : ajout de l'étape 7 (5 phases,
  livrables, critères de validation, commit types)

**Tags créés :**
- `v0.7.1-proxmox-final` (commit `81a9bcc`)

**Commits :**
- `88eefdb` feat(broker): opérations d'écriture Proxmox (échelon B)
- `81a9bcc` Merge branch 'feat-pool-dynamique-echelon-A'
- `17c0f22` docs: plan d'action migration Proxmox → KVM/libvirt
- `4cb0e93` docs: étape 7 — migration Proxmox → KVM/libvirt

---

### Phase 1 — v0.7.2 : fix session portail + robustesse (24-27 août 2026)

**Objectif :** corriger deux bugs indépendants de l'hyperviseur avant la
divergence libvirt, et améliorer la robustesse de la connexion vsock.

#### Bug 1 — `notify_pointer_motion_absolute` échouait (curseur figé)

**Symptôme :** le bureau distant s'affichait, le clavier fonctionnait,
le clic droit ouvrait un menu (toujours au même endroit), mais le clic
gauche semblait inactif et le curseur invité ne bougeait pas.

**Diagnostic :** l'agent créait deux sessions D-Bus portail XDG
séparées — une pour ScreenCast (capture), une pour RemoteDesktop
(injection). Le `stream_node` de la session RemoteDesktop n'était jamais
réellement activé (pas de `open_pipe_wire_remote` sur cette session),
donc GNOME rejetait silencieusement tous les appels
`notify_pointer_motion_absolute`. Les appels ne nécessitant pas de
`stream_node` fonctionnaient (clavier via `notify_keyboard_keycode`,
clics via `notify_pointer_button`), créant l'illusion que seul le clic
gauche était cassé (en réalité les clics étaient injectés à la position
figée du curseur invité, pas là où l'utilisateur cliquait).

**Cause technique :** la méthode `start()` d'une session RemoteDesktop
isolée ne retourne pas les streams ScreenCast (ou retourne un
`stream_node` invalide → 0 via `unwrap_or(0)` ligne 167 de
`remote_desktop.rs`). Le bug était probablement latent depuis
l'implémentation de l'étape 6i (jamais testé end-to-end sur l'injection
avant cette session). Il fonctionnait "par chance" sur certaines configs
(QXL sur Proxmox, où le stream_node=0 pouvait correspondre au seul
framebuffer).

**Correction :** nouveau module `nidan-agent/src/portal_session.rs` qui
négocie UNE session portail unique :

1. `RemoteDesktop::create_session()` → session propriétaire
2. `select_devices(Keyboard | Pointer)` → capabilities d'injection
3. `select_sources(Monitor)` → source ScreenCast SUR LA MÊME session
4. `start()` → une seule popup GNOME, un seul token de restauration
5. `open_pipe_wire_remote()` → activation du stream côté GNOME
6. Le `stream_node` retourné est utilisé à la fois par le capturer
   PipeWire et par la boucle d'injection `notify_pointer_motion_absolute`

Le module s'exécute dans un thread dédié qui héberge la boucle
d'injection et maintient vivants le proxy RemoteDesktop et la session
(condition nécessaire pour que les `notify_*` fonctionnent).

**Impact CSPN :** une seule session D-Bus à auditer au lieu de deux,
surface d'attaque réduite. Un seul token de restauration à persister.

#### Bug 2 — panic openh264 sur dimensions impaires

**Symptôme :** crash du proxy-encoder (`assertion left == right failed:
width needs to be multiple of 2`) quand la capture produisait des
dimensions impaires (ex. 821×536 avec virtio-gpu, résolution liée à la
taille de la fenêtre console Cockpit/virt-viewer).

**Correction :** clamp défensif dans `Openh264Encoder::new()` :
`width = params.width & !1u32; height = params.height & !1u32;`. Un
`warn!` tracing est émis si le clamp est effectivement appliqué. La
fonction `bgra_to_rgb` borne déjà par la plus petite des deux tailles
(data reçue vs `width*height*4`), donc le clamp est sûr.

#### Amélioration — retry vsock avec backoff exponentiel

**Symptôme :** l'agent démarrait avant le proxy-encoder (boot de la VM
avant allocation de session par le broker), recevait `ECONNRESET` ou
`ECONNREFUSED`, mourait, systemd le relançait → boucle de redémarrages
rapides toutes les 2 secondes, polluant les logs et consommant des
ressources.

**Correction :** retry loop intégré dans `main.rs` autour de
`connect_and_handshake()`, avec backoff exponentiel 2s → 4s → 8s → 16s
→ 30s (plafonné). Un `warn!` tracing est émis à chaque tentative avec
`host_cid`, `port`, et délai avant prochain retry. Dès que le proxy est
prêt, l'agent se connecte automatiquement sans intervention manuelle.

**Validation terrain :** proxy-encoder arrêté manuellement → l'agent
attend avec ses WARN réguliers → proxy-encoder redémarré → l'agent se
connecte automatiquement au prochain retry.

#### Commits (branche `fix/v0.7.2-portal-session-unique`) :

1. `98ba7e4` feat(agent): module portal_session pour session XDG unifiée
2. `2f6f096` refactor(agent): constructeurs from_shared_* pour session partagée
3. `a506f91` feat(agent): main.rs bascule sur session portail unifiée
4. `763ae61` fix(proxy-encoder): clamp openh264 dimensions paires
5. `d0ad26f` fix(agent): retry vsock avec backoff exponentiel

**Merge :** `4a0e5db` Merge branch 'fix/v0.7.2-portal-session-unique'

**Tag :** `v0.7.2-etape6j-portal-unifie`

#### Méthode de travail

Les patches ont été appliqués via un script Python automatisé
(`phase1-apply.py`) qui :
- vérifie l'applicabilité de chaque patch avant toute modification
- backup les fichiers cibles
- applique les 9 patches en séquence
- compile en 2 modes (stub + wayland) pour valider les deux branches
  conditionnelles
- crée 4 commits atomiques (1 par sujet)
- push la branche
- en cas d'erreur de compilation : rollback automatique

Ce workflow sera réutilisé pour les phases suivantes.

---

## État actuel du repo

```
main (HEAD)
├── v0.7.2-etape6j-portal-unifie    ← on est ici
├── v0.7.1-proxmox-final            ← point de restauration Proxmox
├── v0.7.1-etape6i-multisession-inputs
├── v0.7-etape6-stable-pre_production
└── v0.6-etape5C2-v2-interactive
```

**Ce qui fonctionne en production (v0.7.2) :**

- Pipeline vidéo bout-en-bout : capture Wayland (PipeWire) → vsock →
  proxy-encoder (openh264 H.264) → QUIC/mTLS/E2E ChaCha20 → client SDL2
- Injection d'inputs : clavier (scancode SDL→evdev) + souris (position
  absolue pixel-parfait via portail RemoteDesktop) + scroll + clic
  droit/gauche — tout via une session D-Bus unique
- Multi-session clients successives sans redémarrage du proxy
- Reconnexion agent automatique après coupure vsock (retry + backoff)
- Token de restauration portail persisté (zéro popup après la première
  autorisation)
- Robustesse aux dimensions impaires (clamp openh264)

**Ce qui ne fonctionne pas encore :**

- Pool dynamique de VMs (échelons C-G) — l'orchestration est manuelle
- Provisionnement de VMs (clone, start, stop, delete) — les échelons
  A+B existent pour Proxmox mais pas encore pour libvirt
- Multi-session simultanée (plusieurs clients sur des VMs différentes
  en parallèle) — nécessite le pool dynamique avec CIDs vsock distincts
- Packages `.deb` (reportés en v2.1/v3)
- VM jetable avec snapshot restauré (mode Sanzu original, reporté)

---

## Ce qui va être fait — Phases 2, 3 et 4

### Phase 2 — Trait `VmProvider` (estimé 0.5 jour)

**Objectif :** découpler le broker de l'hyperviseur pour que les
échelons C-G s'écrivent une seule fois, contre un trait, et fonctionnent
avec n'importe quel backend (libvirt aujourd'hui, Proxmox en option).

**Branche :** `feat-libvirt-provider` depuis `main`

**Livrables :**

- `nidan-broker/src/provider/mod.rs` : trait `VmProvider` extrait
  depuis la signature de fait de `ProxmoxClient` (7 méthodes)

  ```rust
  #[async_trait]
  pub trait VmProvider: Send + Sync {
      async fn clone_vm(&self, template: &str, new_id: &str, name: &str) -> Result<()>;
      async fn start_vm(&self, id: &str) -> Result<()>;
      async fn stop_vm(&self, id: &str) -> Result<()>;
      async fn delete_vm(&self, id: &str) -> Result<()>;
      async fn get_vm_status(&self, id: &str) -> Result<VmStatus>;
      async fn list_vms(&self) -> Result<Vec<VmInfo>>;
  }
  ```

  Note : `wait_for_task` supprimé côté trait car libvirt est synchrone
  (plus de polling UPID). `set_config` supprimé car le CID vsock est
  contrôlé via le XML de domaine, pas via un `args` QEMU.

- `nidan-broker/src/provider/proxmox.rs` : code existant renommé
  depuis `nidan-broker/src/proxmox/mod.rs`, conservé derrière
  `#[cfg(feature = "proxmox-provider")]`

- Le broker consomme `Box<dyn VmProvider>` sélectionné par config TOML :
  ```toml
  [provider]
  backend = "libvirt"  # ou "proxmox"
  ```

### Phase 3 — `LibvirtProvider` : équivalent échelons A+B (estimé 1 jour)

**Objectif :** implémenter le provider libvirt pour piloter les VMs
depuis le broker, avec la même couverture fonctionnelle que le
`ProxmoxClient` validé le 16-17 juillet.

**Livrables :**

- `nidan-broker/src/provider/libvirt.rs` : implémentation avec le
  crate `virt` sur `qemu:///system`

- Clone par backing file qcow2 (`qemu-img create -b`) + génération
  XML de domaine avec CID vsock unique par VM :
  ```xml
  <vsock model='virtio'><cid auto='no' address='{200 + offset}'/></vsock>
  ```

- Template : recréer l'équivalent de la VM template 116 côté libvirt
  (qcow2 de base + XML template avec placeholder CID). Les tokens de
  portail persistés dans l'image restent valables.

- Validation : cycle complet clone → start → get_status → stop → delete

**Gain attendu vs Proxmox :**
- Clone quasi instantané (backing file vs clone complet de 2 min)
- CIDs dynamiques → multi-session simultanée sur un seul hôte
- Socket Unix local → plus d'API HTTP à sécuriser
- `wait_for_task` éliminé → code plus simple

### Phase 4 — Échelons C-G sur le trait (planning inchangé)

Reprise exacte de la roadmap du 16-17 juillet, écrite contre le trait :

- **C** — Allocation VMID (plage 200-299) + `VmPool::assign()` :
  provisionnement auto quand aucune VM statique disponible
- **D** — Destruction automatique post-session dans `VmPool::release()`
- **E** — Pool chaud (`min_available` de `PoolConfig`, déjà présent,
  inutilisé) — **désormais multi-VM possible** grâce aux CIDs dynamiques
- **F-G** — GC des VMs orphelines, quotas, nettoyage au démarrage

**Estimation totale Phases 2+3+4 :** 3 à 4 jours de développement
effectif.

---

## Découvertes techniques notables de la session

### Migration VM Proxmox → KVM/libvirt

- `qm disk export` n'existe pas dans Proxmox — utiliser
  `qemu-img convert` directement sur le zvol ZFS
- La commande `virt-install` nécessite des identifiants OS précis
  (`--osinfo ubuntu22.04`) et une machine compatible
  (`--machine pc` au lieu de `pc-i440fx`)
- Le réseau libvirt (`virsh net-list`) utilise `--network network=xxx`
  (pas `bridge=xxx`) — distinction bridge Linux natif vs réseau libvirt
- Le driver graphique virtio-gpu expose des résolutions dynamiques
  (liées à la fenêtre console), contrairement à QXL/Bochs qui fixent
  des résolutions standards

### Architecture portail XDG (GNOME/Wayland)

- `notify_pointer_motion_absolute` exige un `stream_node` d'un
  ScreenCast **actif** (ouvert via `open_pipe_wire_remote`) sur la
  **même session D-Bus** que l'injecteur
- Deux sessions D-Bus séparées (une ScreenCast, une RemoteDesktop)
  donnent des `stream_node` incompatibles — GNOME rejette
  silencieusement les injections absolues
- Le token de restauration RemoteDesktop doit être demandé via
  `PersistMode::ExplicitlyRevoked` sur `select_devices`, pas sur
  `select_sources` (qui refuse `PersistMode != DoNot` sur une
  session RemoteDesktop combinée)
- La méthode `session.path()` est `pub(crate)` dans ashpd 0.9 —
  inaccessible depuis du code consommateur

### Robustesse encodeur H.264

- openh264 panic si les dimensions ne sont pas des multiples de 2
  (contrainte YUV 4:2:0) — clamp défensif `& !1u32` nécessaire
- `bgra_to_rgb` borne intelligemment par `min(data.len, w*h*4)`,
  donc les dimensions clampées sont sûres même si la frame originale
  est légèrement plus grande

---

## Fichiers créés ou modifiés (Phase 1)

| Fichier | Action |
|---|---|
| `nidan-agent/src/portal_session.rs` | **Nouveau** — session portail unifiée |
| `nidan-agent/src/main.rs` | Modifié — bascule sur session partagée + retry vsock |
| `nidan-agent/src/capture/pipewire.rs` | Modifié — `from_shared_stream()` |
| `nidan-agent/src/remote_desktop.rs` | Modifié — `from_shared_channel()` + `pub(crate) sdl_scancode_to_evdev` |
| `nidan-proxy-encoder/src/encoder/openh264_enc.rs` | Modifié — clamp dimensions paires |
| `docs/plan-action-migration-kvm.md` | **Nouveau** — décision d'architecture |
| `plan-dev-v2.md` | Modifié — ajout étape 7 |

---

## Prochaine action

Phase 2 (trait `VmProvider`) sur la branche `feat-libvirt-provider`.
Workflow identique : script d'automatisation + validation par étapes.
