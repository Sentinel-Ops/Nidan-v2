# NIDAN — Plan de développement v3

Ce document trace la démarche complète depuis la refonte v1 → v2
(encodeur hors VM, isolation par vsock) jusqu'à l'industrialisation
et les briques d'innovation pour la thèse IDPE. Il sert de
référence commune et de trace des choix.

**Historique :** plan v1 (proto → intégration), plan v2 (migration
KVM/libvirt, pool dynamique), plan v3 (industrialisation, innovation,
fonctionnel).

**État au 4 septembre 2026 :** v0.9.0-warm-pool — pool chaud,
GC orphelines, quotas, 15 fichiers, +630 lignes.

## Principes directeurs

Trois règles qui structurent toute la démarche :

1. **À chaque étape, quelque chose compile et se teste en isolation.** Pas de refactor monolithique de plusieurs jours qui ne compile qu'à la
fin. Chaque étape produit un livrable partiel utilisable.
2. **Après validation d'une étape, on met à jour le repo Nidan-v2.** Chaque étape aboutit à un ou plusieurs commits cohérents avec un
message décrivant l'intention (`feat:`, `docs:`, `chore:`).
3. **On ne casse pas ce qui marche.** Le code v1 est figé sur
[Sentinel-Ops/Nidan](https://github.com/Sentinel-Ops/Nidan) au tag
`v1.0-fonctionnelle`. La v2 démarre de cet état comme base.

## Vue d'ensemble — 7 étapes

| # | Étape                                                 | Livrable                                            | Durée estimée   |
| --- | ----------------------------------------------------- | --------------------------------------------------- | --------------- |
| 1 | Cadrer le protocole vsock (Protobuf)                  | `nidan-proto/proto/agent.proto`                     | Quelques heures |
| 2 | Prototype vsock isolé (validation canal)              | 2 binaires de test                                  | Quelques heures |
| 3 | Créer `nidan-proxy-encoder` avec source factice       | Crate qui compile + flux test bout-en-bout          | 1 jour          |
| 4 | Créer `nidan-agent` (allègement de `nidan-server` v1) | Binaire qui envoie pixels bruts sur vsock           | 1 jour          |
| 5 | Intégration bout-en-bout                              | Chaîne complète client ↔ proxy ↔ vsock ↔ agent ↔ VM | 0.5 à 1 jour    |
| 6 | Documentation Proxmox + robustesse + polissage        | Guide de déploiement + `.deb` + multi-session       | 1 à 2 jours     |
| 7 | Migration Proxmox → KVM/libvirt + pool dynamique      | Trait `VmProvider`, `LibvirtProvider`, échelons C-G | 3 à 4 jours     |
| 8 | Industrialisation                                      | Systemd, template optimisé, docs déploiement KVM    | 2 à 3 jours     |
| 9 | Innovation thèse IDPE (C2-E)                           | Firewall sémantique, capacités CID, IMA/EVM         | 3 à 5 jours     |
| 10 | Fonctionnel avancé                                    | OIDC/MFA, PKI automatisée, multi-nœud               | 4 à 6 jours     |

**État v0.9.0** : étapes 1-7 terminées (4 septembre 2026). Étapes 8-10
constituent la roadmap v3.

Estimation totale initiale : **4 à 5 jours de développement effectif**, hors
allers-retours de validation en environnement réel. L'étape 6 s'est étendue
au-delà de l'estimation initiale suite à la découverte et correction d'un
bug de fond dans le pipeline d'encodage (voir détail plus bas). L'étape 7
a été ajoutée après la décision d'abandonner Proxmox au profit d'une stack
KVM/libvirt pure (voir `docs/plan-action-migration-kvm.md` pour la décision
d'architecture détaillée).

---

## Étape 1 — Cadrer le protocole vsock

### Objectif

Figer le format des messages Protobuf qui circulent sur vsock entre `nidan-agent` (VM) et `nidan-proxy-encoder` (hôte). C'est le contrat sur
lequel les deux composants sont écrits.

### Décisions déjà prises

- **Format pixels** : RGBA brut (pas de conversion couleur côté agent).
→ agent VM le plus simple possible = moindre surface d'attaque dans la VM.
- **Frames complètes** (pas de damage tracking).
→ comme Sanzu (page 12) ; simplicité et sécurité.
- **Proxy commande l'agent** (`StartCapture` / `StopCapture`).
→ le proxy maîtrise le flux (démarrage, arrêt, reconfiguration).
- **Réutilisation d'`InputBatch`** du proto v1 pour les entrées retour.
→ zéro travail de proto, sémantique déjà validée.
- **Transport pixels VM ↔ hôte** : vsock (AF_VSOCK).

### Messages à définir

- `AgentHello` : première trame de l'agent, annonce version et capacités.
- `StartCapture` / `StopCapture` : contrôle de session (proxy → agent).
- `RawFrame` : une frame de pixels bruts (agent → proxy).
- `PixelFormat` (enum) : RGBA8, BGRA8, extensible.
- `AgentMessage` : enveloppe `oneof` pour multiplexer sur le flux vsock.
- Réutiliser `InputBatch` (nidan-proto v1) pour les entrées retour.

### Framing

`[u32 length little-endian][protobuf bytes]` sur le flux vsock, comme le
canal de contrôle QUIC actuel.

### Livrable

- Nouveau fichier `nidan-proto/proto/agent.proto` dans le repo.
- README v2 mis à jour si besoin (référence au proto).

### Critère de validation

Le fichier `.proto` compile avec `protoc`, il est cohérent avec les cas
d'usage listés, et je le valide avec toi avant de passer à l'étape 2.

### Commit type

```
feat(proto): définir agent.proto pour le canal vsock v2

- messages AgentHello, StartCapture, StopCapture, RawFrame
- PixelFormat en enum (RGBA8, BGRA8)
- enveloppe AgentMessage (oneof) pour multiplexage
- réutilisation d'InputBatch pour les entrées retour
```

---

## Étape 2 — Prototype vsock isolé

### Objectif

Valider en isolation que le canal vsock fonctionne entre la VM Ubuntu et
l'hôte Proxmox, **avant** d'intégrer vsock dans le code NIDAN.

### Livrable

Deux petits binaires Rust indépendants du projet NIDAN :

- `vsock-sender` : à lancer dans la VM, génère un flux de test (pattern
de pixels) et l'envoie sur vsock.
- `vsock-receiver` : à lancer sur l'hôte Proxmox, reçoit le flux et
mesure débit / latence / pertes.

### Ce qu'on valide

- Le module noyau `vhost_vsock` est chargé sur l'hôte.
- Le device `vhost-vsock-pci` est correctement configuré sur la VM
(via l'UI Proxmox ou l'édition de la conf QEMU).
- La crate Rust `vsock` (ou `tokio-vsock`) fonctionne comme attendu.
- Le débit encaisse ~250 Mo/s (débit brut d'un flux 1080p30 RGBA).
- La latence est acceptable (typiquement < 1 ms pour vsock local).

### Contraintes de travail

**Je ne peux pas tester vsock dans mon environnement** (pas de KVM
disponible). Je fournis les deux binaires et un guide de test ; toi tu
les lances sur ton setup Proxmox et tu me remontes les résultats. C'est
l'étape où on aura besoin d'un aller-retour rapide.

### Commit type

```
chore(prototype): binaires de test vsock (validation canal isolé)

- vsock-sender : générateur de flux dans la VM
- vsock-receiver : lecteur + métriques sur l'hôte
- guide de test dans docs/vsock-prototype.md
```

---

## Étape 3 — Créer `nidan-proxy-encoder`

### Objectif

Créer le nouveau crate `nidan-proxy-encoder` qui tournera sur l'hôte
Proxmox. À la fin de cette étape, il expose un service QUIC + mTLS +
E2E ChaCha20 au client (comme le serveur v1), encode en H.264, mais lit
ses frames depuis une **source factice** (pas encore l'agent réel).

### Base de départ

~70 % du code de `nidan-server` v1 est réutilisable :

- serveur QUIC (quinn) + mTLS ;
- handshake E2E (X25519 + HKDF-SHA256) ;
- encodeur H.264 (openh264) ;
- gestion de session + JWT.

Ce qui change :

- la source des frames n'est plus la capture Wayland locale, mais une
fonction abstraite (trait `FrameSource`) ;
- pour l'étape 3, on branche une implémentation factice de `FrameSource` (dégradé animé, pattern de test).

### Livrable

- Nouveau crate `nidan-proxy-encoder` (dans le workspace).
- Trait `FrameSource` défini proprement.
- Implémentation `FrameSource::Test` (pattern de pixels animé).
- Binaire qui compile et écoute en QUIC.

### Critère de validation

Depuis le client v1 actuel (ou une build légèrement adaptée), on peut se
connecter au `nidan-proxy-encoder` sur l'hôte et voir le pattern de test
s'afficher, chiffré E2E. À ce stade :

- l'E2E fonctionne ;
- l'encodage H.264 fonctionne ;
- la face QUIC / client fonctionne.

Il ne manque que la vraie source (l'agent).

### Commit type

```
feat(proxy-encoder): nouveau crate, source factice validée

- extraction du serveur QUIC/mTLS/E2E depuis nidan-server v1
- extraction de l'encodeur H.264
- trait FrameSource + implémentation Test (pattern animé)
- binaire qui expose le service QUIC au client
```

---

## Étape 4 — Créer `nidan-agent`

### Objectif

Créer `nidan-agent` qui tournera dans la VM Ubuntu, à partir du code de `nidan-server` v1 dont on retire tout ce qui n'a plus sa place.

### Ce qu'on retire de `nidan-server` v1

- L'encodeur H.264 (transféré au proxy).
- Le serveur QUIC face client (le client ne parle plus à la VM).
- La gestion des sessions et le JWT (fait par le broker/proxy).
- Le presse-papier serveur si applicable (à revoir en étape 5).

### Ce qu'on garde

- La capture Wayland via portail PipeWire/ScreenCast (module `capture`).
- L'injection RemoteDesktop (module `remote_desktop` + fix scancode→evdev).
- La lecture de config, les logs, la gestion Ctrl+C.

### Ce qu'on ajoute

- Un module de sortie vsock : ouverture d'une connexion vsock vers
l'hôte (CID = 2), envoi des frames au format `RawFrame` défini en
étape 1, réception des `InputBatch` en retour.
- Le protocole d'étape 1 (`AgentHello`, `StartCapture`, etc.).

### Livrable

- Crate `nidan-agent` (remplace `nidan-server` dans le workspace v2).
- Binaire qui, une fois lancé dans la VM, ouvre vsock vers l'hôte et
attend `StartCapture`.

### Critère de validation

Depuis l'hôte, avec un outil simple (`vsock-cat` ou un binaire de test),
on peut envoyer un `StartCapture` et recevoir un flux de `RawFrame` en
retour, correspondant au bureau Wayland de la VM.

### Commit type

```
feat(agent): nouveau crate nidan-agent (capture Wayland + sortie vsock)

- allègement de nidan-server v1 : retrait encodeur, QUIC, JWT
- ajout de la sortie vsock (RawFrame vers hôte)
- ajout de l'entrée vsock (InputBatch depuis hôte)
- conservation de la capture Wayland et de l'injection RemoteDesktop
```

---

## Étape 5 — Intégration bout-en-bout

### Objectif

Brancher l'agent (étape 4) au proxy-encoder (étape 3) en remplaçant la
source factice par la vraie source vsock. Tester la chaîne complète en
réel.

### Ce qu'on fait

- Dans `nidan-proxy-encoder`, ajouter `FrameSource::Vsock` qui se
connecte à l'agent VM (CID de la VM, port fixé).
- Faire dialoguer proxy et agent selon le protocole d'étape 1.
- Tester : client → proxy-encoder (hôte) → vsock → agent (VM) → capture
Wayland de la VM affichée sur le client.

### Points de vigilance identifiés

- **Format de pixels PipeWire vs openh264** : PipeWire peut renvoyer
BGRA ou RGBA selon le compositeur ; l'agent doit annoncer le bon
`PixelFormat` et le proxy adapter l'encodage.
- **Timing et buffering** : à ~250 Mo/s, un pipeline mal buffé peut
provoquer des à-coups. Prévoir une queue bornée entre vsock et
encodeur.
- **Backpressure** : si l'encodeur est plus lent que l'agent, il faut
soit sauter des frames côté agent, soit compter sur le contrôle de
flux vsock.
- **Reconnexion** : que se passe-t-il si l'agent redémarre pendant une
session ? Prévoir un état « waiting for agent » côté proxy.

### Livrable

Une chaîne complète fonctionnelle. On peut voir le bureau de la VM sur
le poste client, chiffré E2E, avec les entrées clavier/souris qui
fonctionnent.

### Critère de validation

Test bout-en-bout réussi sur ton infra Proxmox. C'est le moment où on
retrouve l'état fonctionnel de la v1, mais avec l'encodeur hors VM.

### Commit type

```
feat(integration): source vsock dans le proxy + connexion à l'agent

- FrameSource::Vsock : lecture des frames depuis l'agent
- gestion StartCapture/StopCapture selon le protocole agent.proto
- gestion du format de pixels (BGRA/RGBA) reçu de PipeWire
- reconnexion propre en cas de redémarrage de l'agent
```

---

## Étape 6 — Documentation Proxmox + robustesse + polissage

### Objectif

Rendre la solution déployable par un tiers, avec un guide clair, et
corriger les limitations connues issues des étapes précédentes
(mono-session, résolution figée, robustesse du pipeline vidéo).

### Découpage en blocs

L'étape 6 a été découpée en blocs indépendants plutôt qu'un unique
livrable monolithique, pour pouvoir livrer et valider progressivement :

- **Bloc 1** — Documentation Proxmox (déploiement reproductible)
- **Bloc 2** — Services systemd (proxy, broker, agent)
- **Bloc 3** — Packages `.deb` (reporté, non prioritaire)
- **Bloc 4** — Fix multi-session VsockService
- **Bloc 5** — VM jetable Sanzu-style avec snapshot (reporté, v2.1/v3)

### Livrables

- `docs/DEPLOIEMENT-PROXMOX.md` : guide complet 12 sections
(architecture, prérequis, hôte, VM invitée, PKI, réseau, snapshots,
dépannage).
- `docs/INSTALLATION-HOTE.md` / `docs/INSTALLATION-VM.md` : guides
condensés par machine.
- `docs/systemd/nidan-proxy-encoder.service`, `nidan-broker.service`,
`nidan-agent.service` (unité **utilisateur**, pas système, requise
pour l'accès au portail Wayland via D-Bus utilisateur).

### Critère de validation

Un binôme extérieur pourrait déployer NIDAN v2 sur son propre Proxmox
en suivant les docs, sans avoir à demander.

---

### Bilan détaillé — corrections apportées en étape 6

Au-delà de la documentation initialement prévue, l'étape 6 a englobé une
session de debug approfondie suite à des régressions et limitations
identifiées en usage réel prolongé. Le détail est conservé ici pour
traçabilité, chaque point ayant fait l'objet d'un commit dédié sur la
branche `main`.

**Fix agent — Ctrl+C propre + persistance des autorisations portail**
(commit `f6487f5`)

- Ctrl+C ne rendait jamais la main : le timer de vérification du
shutdown dans la mainloop PipeWire (`capture/pipewire.rs`) était
enregistré via `add_timer()` mais jamais armé avec un intervalle
(`update_timer()` manquant). Fix : timer armé à 200 ms.
- Popup d'autorisation à chaque démarrage de l'agent : ScreenCast et
RemoteDesktop chargent et sauvegardent désormais leur token de
restauration dans `~/.local/state/nidan-agent/`. Après une
autorisation manuelle unique (à faire lors de la préparation du
template VM), les démarrages suivants sont silencieux.

**Fix couleurs — BGRA→RGB inversé** (commit `b3eff23`)

- Le portail Wayland négocie explicitement du BGRA avec PipeWire, mais
le buffer livré au capturer contient en réalité l'ordre RGBA. La
fonction de conversion inversait donc R et B pour rien. Confirmé
visuellement (logo Leboncoin orange au lieu de bleu après fix).

**Fix multi-session — VsockService via broadcast** (commit `2e09044`)

- Le canal de frames entre l'agent et le proxy était un `mpsc`
mono-consommateur, "pris" une seule fois. Après une déconnexion
client, il fallait redémarrer tout le proxy pour retester —
limitation documentée depuis l'étape 5B, jamais corrigée jusque-là.
- Remplacé par un canal `broadcast` avec tâche de fan-out permanente :
chaque nouvelle session cliente s'abonne indépendamment
(`subscribe_frames_as_mpsc()`), sans affecter les sessions passées ou
futures. Validé par sessions client successives sans redémarrage du
proxy.

**Fix résolution dynamique + keep-alive QUIC + robustesse client**
(commit `37707bd`)

Cinq correctifs combinés, du plus défensif au plus fondamental,
trouvés lors d'une session de debug approfondie sur un freeze vidéo
récurrent (10 à 60 secondes de tenue avant ces fixes) :

1. **Résolution dynamique** — le proxy annonçait 1920×1080 par défaut
au client au lieu des vraies dimensions capturées par l'agent
(ex. 1280×800). `VsockService::wait_for_agent_capabilities()`
attend maintenant les vraies dimensions négociées via `AgentHello`
avant de répondre au client (timeout 30s).
2. **Keep-alive QUIC** — ni le proxy ni le client ne configuraient de
`TransportConfig` (défaut quinn : idle timeout 10s, keep-alive
désactivé). Ajout de `max_idle_timeout=60s` et
`keep_alive_interval=5s` des deux côtés.
3. **Retrait du VSync SDL2** — `canvas.present()` avec VSync pouvait
bloquer indéfiniment lors d'un changement d'écran (bug connu
SDL2/OpenGL sous Linux). Rendu et capture souris/clavier étant dans
la même boucle séquentielle côté client, un blocage gelait les
deux à la fois. Piste réelle mais insuffisante seule pour expliquer
tous les freezes observés.
4. **Timeouts défensifs côté client** — l'envoi des `InputBatch` et
l'envoi des frames vers le décodeur se faisaient via des
`.send().await` / `write_all()` sans timeout dans la boucle
`select!` principale. Ajout d'un timeout de 5s sur les deux,
transformant un gel silencieux et infini en échec propre et
observable (log d'erreur explicite).
5. **Cause racine du freeze — flux H.264 100 % IDR** — `is_keyframe`
était codé en dur à `true` pour **toutes** les frames relayées par
l'agent (`capture/vsock.rs`). Cascade : `encoder/mod.rs` forçait
`request_keyframe()` à chaque frame, qui forçait
`force_intra_frame()` dans `openh264_enc.rs` à chaque frame. Le
proxy encodait donc **chaque frame en IDR complet, jamais de
P-frame** — un flux H.264 100 % intra, usage extrêmement atypique
du codec. La documentation du crate `openh264` indique explicitement
qu'un encodeur au comportement "exotique" peut produire des flux que
leur décodeur ne gère pas robustement — cohérent avec le décodage
anormalement lent observé (120-150 ms/frame, typique d'un flux
100 % intra) et le blocage silencieux du décodeur après quelques
dizaines de secondes. **Fix** : seule la toute première frame de la
session est marquée keyframe ; le reste suit le cycle périodique
normal de l'encodeur (~20 frames) — flux H.264 IDR+P standard.

**Validation finale** : session de 14+ minutes avec interaction active
continue (clics, mouvements souris), 5580+ frames envoyées, `kf=false`
sur l'immense majorité des frames comme attendu, aucun freeze, aucun
timeout déclenché. Arrêt final volontaire (Ctrl+C), pas un crash.

**Fix relais inputs sur reconnexions vsock — multi-session inputs**
(commit `85e03f3`, tag `v0.7.1-etape6i-multisession-inputs`)

Bug découvert après validation en usage prolongé : à partir de la 2ᵉ
connexion agent au proxy (soit après un simple restart de l'agent, soit
sur une reconnexion agent suite à une coupure vsock), la session
s'établissait, la vidéo s'affichait normalement, mais **aucune
interaction clavier/souris n'était plus injectée dans la VM**. Côté
agent, un WARN systématique : `nidan_agent::vsock_link: erreur lecture
vsock — fermeture de la boucle reader error=lecture longueur`.

**Cause racine** dans `nidan-proxy-encoder/src/capture/vsock.rs` :

- Le champ `inputs_rx: Arc<Mutex<Option<Receiver>>>` était consommé via
`guard.take()` à la première session, et **jamais repeuplé** malgré le
commentaire qui décrivait l'intention. Les sessions suivantes récupéraient
donc `None`.
- La branche `else` de `run_session` (en cas de `None`) faisait un
`writer.shutdown().await` — un half-close TCP-like côté proxy. Côté
agent, ce half-close se traduisait par un EOF immédiat sur le
`read_exact(len)` du reader vsock, ce qui tuait sa `reader_task`.
- Résultat : le canal proxy → agent était fonctionnellement coupé
dès la 2ᵉ session. Aucun `AgentMessage::Inputs` ne pouvait plus
descendre. Frames agent → proxy toujours OK (writer côté agent
intact), d'où le symptôme : image affichée mais interaction morte.

**Correction** :

- Champ transformé en `Arc<Mutex<Receiver>>` (sans `Option`) : le
receiver vit pour toute la durée du process, ce qui préserve les
`inputs_tx` déjà clonés et distribués au `VsockService`.
- À chaque nouvelle session, on prête un clone de l'`Arc<Mutex>` à
`run_session`. La boucle de relais garde le `MutexGuard` pendant
toute sa durée puis le libère au drop — la prochaine session peut à
son tour lock et relayer, sans recréation de canal.
- Comme le `VsockCapturer` est mono-session par VM cible (une seule
connexion agent active à la fois), la contention sur le Mutex est
nulle en pratique.
- La branche `else { writer.shutdown() }` a été supprimée : elle
n'a plus de raison d'être puisque le receiver est toujours disponible.
- La boucle de relais est extraite en `run_inputs_relay<W: AsyncWrite>`,
générique sur le transport, ce qui permet trois tests unitaires
(`tokio::io::duplex`) sans dépendre du kernel vsock :
  * `inputs_relay_survives_multiple_sessions` — régression directe du bug.
  * `inputs_relay_terminates_on_shutdown` — libération propre du guard.
  * `inputs_relay_preserves_batch_bytes` — framing bit-exact préservé.

**Validation terrain** : reconnexion agent puis session cliente sans
redémarrage du proxy → inputs fonctionnels, plus aucun WARN
`lecture longueur` côté agent, `InputBatch relayé sur vsock` visible en
`RUST_LOG=nidan_proxy_encoder::capture::vsock=debug` à chaque frappe.

---

## Étape 7 — Migration Proxmox → KVM/libvirt + reprise pool dynamique

### Contexte du pivot

L'infrastructure de développement bascule d'une stack Proxmox vers du
KVM/libvirt pur, hébergée sur un serveur Dell R440. Décision prise après
la session de développement du 16-17 juillet 2026 sur le pool dynamique
Proxmox (échelons A+B validés) et le constat en usage que Proxmox impose
plusieurs limitations structurelles pour l'usage NIDAN :

- Attribut `args` réservé root → CID vsock non modifiable par API →
  contrainte mono-session par hôte
- Couche d'abstraction Proxmox (pveproxy, pvedaemon, storage plugins)
  qui augmente la surface d'attaque pour la CSPN
- API HTTP à sécuriser (mTLS, épinglage cert) vs socket Unix local
  libvirt = surface de sécurité plus large
- Dépendance à un produit tiers qui affaiblit le narratif souveraineté

Le passage à libvirt lève ces trois contraintes simultanément : le CID
vsock devient un élément du XML de domaine entièrement contrôlé par le
broker, le multi-session dynamique redevient possible sur un seul hôte,
et le TOE CSPN se resserre autour de composants souverains bien connus.

### Décision d'architecture — ni fork, ni nouveau repo

Le repo `Sentinel-Ops/Nidan-v2` est conservé unique. Introduction d'un
**trait `VmProvider`** qui découple le broker de l'hyperviseur, avec deux
implémentations :

- `LibvirtProvider` — nouvelle implémentation cible (crate `virt`, socket
  Unix local `qemu:///system`)
- `ProxmoxProvider` — code existant conservé derrière un feature flag
  Cargo `proxmox-provider`, non compilé par défaut, hors périmètre CSPN,
  réactivable si un cas d'usage commercial le demande

Justification complète dans `docs/plan-action-migration-kvm.md`.

### Découpage en 5 phases

**Phase 0 — Sanctuarisation Git (fait)**

- Tag `v0.7.1-etape6i-multisession-inputs` sur `main`
- Merge de `feat-pool-dynamique-echelon-A` (échelon B finalisé, test
  d'intégration retiré pour éviter les secrets en dur)
- Tag `v0.7.1-proxmox-final` : point de restauration permanent pour la
  version Proxmox fonctionnelle
- Commit du document `docs/plan-action-migration-kvm.md`

**Phase 1 — v0.7.2 : fix session portail unique (en cours)**

À faire **avant** la divergence libvirt car indépendant de l'hyperviseur.
Deux fixes qui doivent rester dans le tronc commun :

1. Nouveau module `nidan-agent/src/portal_session.rs` : négociation
   portail XDG unique (RemoteDesktop + ScreenCast couplés sur UNE
   session D-Bus). Corrige le bug `notify_pointer_motion_absolute` qui
   échouait à cause de deux sessions D-Bus séparées créant des
   `stream_node` non valides. Effet observé avant fix : curseur figé,
   clic droit et clavier fonctionnels (position-indépendants), clic
   gauche apparemment inactif (en réalité injecté à la position figée
   du curseur invité).
2. Clamp openh264 dimensions paires dans `nidan-proxy-encoder` :
   robustesse à toute résolution capture. Bug révélé par le passage
   virtio-gpu à 821×536 (panic `width needs to be multiple of 2`).
   À corriger défensivement quel que soit le driver graphique.

Bonus : une seule popup GNOME d'autorisation, un seul token de
restauration à gérer, surface D-Bus réduite (argument CSPN).

Tag cible : `v0.7.2-etape6j-portal-unifie`.

**Phase 2 — Trait `VmProvider` (0.5 jour)**

Branche `feat-libvirt-provider` depuis `main`.

- Extraction du trait depuis la signature de fait de `ProxmoxClient`
  (7 méthodes : clone_vm, start_vm, stop_vm, delete_vm, set_config,
  get_vm_status, list_vms — `wait_for_task` supprimé côté trait car
  libvirt est synchrone)
- Déplacement du code Proxmox derrière `#[cfg(feature = "proxmox-provider")]`
- Le broker consomme `Box<dyn VmProvider>` sélectionné via config TOML

**Phase 3 — `LibvirtProvider` : équivalent échelons A+B (1 jour)**

- Implémentation avec le crate `virt` sur `qemu:///system`
- Clone par backing file qcow2 (`qemu-img create -b`) + génération XML
  de domaine avec CID vsock unique par VM (`cid=200+offset`)
- Template : recréer l'équivalent de la VM template 116 côté libvirt
  (qcow2 de base + XML template avec placeholder CID). Les tokens de
  portail persistés dans l'image restent valables (indépendants de
  l'hyperviseur)
- Validation : cycle complet clone → start → get_status → stop → delete,
  équivalent à la validation Proxmox du 16-17 juillet (133 s en Proxmox ;
  attendu significativement plus rapide avec backing files libvirt)

**Phase 4 — Reprise des échelons C-G sur le trait (planning inchangé)**

Reprise exacte de la roadmap prévue en session du 16-17 juillet, mais
écrite contre le trait (donc valable pour tout provider futur) :

- **Échelon C** — Allocation VMID (plage 200-299) + intégration dans
  `VmPool::assign()` : provisionnement automatique quand aucune VM
  statique disponible
- **Échelon D** — Destruction automatique post-session dans
  `VmPool::release()`
- **Échelon E** — Pool chaud (`min_available` de `PoolConfig`, déjà
  présent, inutilisé) — **désormais multi-VM possible** grâce aux CIDs
  dynamiques
- **Échelons F-G** — GC des VMs orphelines, quotas, nettoyage au
  démarrage du broker

### Livrables

- `nidan-agent/src/portal_session.rs` (module session portail unifié)
- `nidan-broker/src/provider/mod.rs` (trait `VmProvider`)
- `nidan-broker/src/provider/libvirt.rs` (implémentation cible)
- `nidan-broker/src/provider/proxmox.rs` (renommé depuis
  `nidan-broker/src/proxmox/mod.rs`, feature-gated)
- `docs/plan-action-migration-kvm.md` (déjà en place)

### Critère de validation

Cycle complet identique à la validation du 16-17 juillet, mais côté
libvirt : provisionnement dynamique d'une VM du pool, session cliente
end-to-end avec inputs, destruction propre post-session. Multi-session
simultanée validée (nouveauté rendue possible par la migration).

### Commit types

```
docs: plan d'action migration Proxmox → KVM/libvirt
fix(agent): session portail XDG unique (RemoteDesktop + ScreenCast)
fix(proxy-encoder): clamp openh264 dimensions paires
refactor(broker): extraction trait VmProvider
feat(broker): LibvirtProvider (échelons A+B, cible primaire)
chore(broker): feature-gate ProxmoxProvider (proxmox-provider)
feat(broker): pool dynamique échelon C — allocation VMID
feat(broker): pool dynamique échelon D — release + destruction
feat(broker): pool dynamique échelon E — pool chaud multi-VM
feat(broker): pool dynamique échelons F/G — GC + quotas
```

---

## Contraintes de mon environnement de travail

Il faut que ce soit dit clairement pour éviter les malentendus :

- **Je ne peux pas exécuter QEMU/KVM.** Donc dès qu'il s'agit de tester
vsock, le déploiement Proxmox, ou l'agent dans une VM, c'est **toi
qui exécutes**, et on communique les résultats.
- **Je peux compiler tout le code Rust** dans un environnement Linux
avec les dépendances système appropriées (installées à la demande :
`libpipewire-0.3-dev`, `libspa-0.2-dev`, `libsdl2-dev`, `clang`,
`protobuf-compiler`), ce qui a permis de vérifier chaque patch de
l'étape 6 par compilation réelle avant livraison — y compris le
linking SDL2 côté client.
- **Je peux tester tout ce qui ne nécessite pas vsock ou Wayland
réels** (proto, encodeur H.264 sur un flux fabriqué, serveur QUIC
avec un client simulé).

Chaque étape marque explicitement ce qui est testable ici vs ce qui
demande ton environnement.

---

## Cadence de mise à jour du repo

Après chaque étape validée :

```
cd ~/Documents/NIDAN_SECURITY/nidan-v2
git add <fichiers de l'étape>
git commit -m "<message décrit dans l'étape>"
git push
```

Un commit par étape (idéalement), ou quelques commits atomiques par
étape si plusieurs sujets distincts. Pas de gros commit fourre-tout.

---

## État actuel

- **Étape 0 (fait)** : repo `Nidan-v2` créé, README de fondation
poussé, principe (vsock, proxy sur l'hôte) documenté.

- **Étape 1 (fait)** : `agent.proto` v2 défini, `prost-build` intégré,
types Rust générés utilisables (`nidan_proto::agent`).

- **Étape 2 (fait)** : canal vsock validé sur Proxmox.

  * VM guest CID=42, hôte CID=2, port 5000.
  * Test 300 frames RGBA 1920×1080 à 30 fps (~2.5 Go transportés).
  * Débit mesuré : **248.7 MB/s** (théorique : 248.8), 0 perte, 0 hors ordre.
  * Le canal vsock encaisse le débit cible sans accumulation de retard.
  * Note : la latence affichée (~32 ms) par le prototype est un artefact
de mesure (décalage d'horloge VM↔hôte non compensé), pas la vraie
latence de transit.

- **Étape 3 (fait)** : crate `nidan-proxy-encoder` créé, face client validée
bout-en-bout.

  * Test réel : client Debian 12 → broker (Ubuntu 20.04)
→ proxy-encoder v2 (dans VM cible 192.168.8.100)
  * Handshake mTLS + JWT + E2E ChaCha20 fonctionnels
  * Encodage H.264 (openh264) + décodage client OK
  * Rendu SDL2 côté client : dégradé RVB du StubCapturer visible

- **Étape 4 (fait)** : crate `nidan-agent` créé, compilation et démarrage
validés dans la VM cible.

  * `main.rs` + `config.rs` + `vsock_link.rs` (nouveau code)
  * Trait `Capturer` v1 réutilisé (StubCapturer, PipeWire feature-gated)
  * Handshake AgentHello ↔ ProxyHelloAck, framing prost sur vsock
  * Envoi RawFrame (pixels bruts) + réception structurée des commandes

- **Étape 5A (fait)** : VsockCapturer côté proxy-encoder, backend
configurable stub/vsock.

- **Étape 5B (fait)** : intégration bout-en-bout validée, modèle Sanzu
fonctionnellement démontré.

  * VsockService global instancié au boot du proxy (modèle A, aligné Sanzu)
  * Test réel bout-en-bout réussi, 14 frames décodées, 0 droppée
  * Preuve : [Release GitHub v0.5-etape5B-sanzu-fonctionnel](https://github.com/Sentinel-Ops/Nidan-v2/releases/tag/v0.5-etape5B-sanzu-fonctionnel)

- **Étape 5C.1 (fait)** : Wayland réel côté agent — le vrai bureau
s'affiche côté client.

  * Capture PipeWire en 1280x800 BGRA
  * Preuve : [Release GitHub v0.5.1-etape5C1-wayland-fonctionnel](https://github.com/Sentinel-Ops/Nidan-v2/releases/tag/v0.5.1-etape5C1-wayland-fonctionnel)

- **Étape 5C.2 (fait)** : relais des inputs client → agent. Le bureau
distant est interactif.

  * 165 frames décodées côté client (0 droppée), 432 InputBatch injectés
  * La v2 est fonctionnellement complète : le modèle Sanzu SSTIC 2022
est concrètement reproduit avec les ajouts propres à ce projet
(E2E ChaCha20, QUIC, JWT/mTLS).
  * Preuve : [release v0.6-etape5C2-v2-interactive](https://github.com/Sentinel-Ops/Nidan-v2/releases/tag/v0.6-etape5C2-v2-interactive)

- **Étape 6, blocs 1+2 (fait)** : documentation Proxmox complète
(12 sections) + services systemd pour les trois composants
(proxy, broker, agent en unité utilisateur).

- **Étape 6, fix agent (fait)** : Ctrl+C propre (timer PipeWire armé) +
persistance des autorisations portail (token ScreenCast/RemoteDesktop
sauvegardé, plus de popup après la première autorisation).

- **Étape 6, fix couleurs (fait)** : correction de l'inversion
BGRA/RGB — PipeWire livre du RGBA malgré la négociation BGRA.

- **Étape 6, bloc 4 — fix multi-session (fait)** : VsockService
refactorisé en canal broadcast + fan-out permanent. Sessions clientes
successives sans redémarrage du proxy.

- **Étape 6, robustesse pipeline vidéo (fait)** : résolution dynamique,
keep-alive QUIC, retrait VSync SDL2, timeouts défensifs côté client,
et **correction de la cause racine du freeze vidéo récurrent** : le
proxy encodait chaque frame en IDR H.264 complet au lieu d'un flux
IDR+P normal (bug `is_keyframe` codé en dur). Validé par une session
de 14+ minutes d'interaction active continue sans freeze ni timeout,
contre 10 à 60 secondes de tenue auparavant.

- **Étape 6i-bis, fix inputs multi-session (fait)** : correction du
relais des `InputBatch` proxy → agent sur les reconnexions vsock.
`Arc<Mutex<Option<Receiver>>>` remplacé par `Arc<Mutex<Receiver>>`
(receiver jamais consommé, prêté aux sessions successives via
`MutexGuard`), suppression de la branche `else { writer.shutdown() }`
qui coupait le canal côté agent. Trois tests unitaires ajoutés
(dont la régression directe). Validé en conditions réelles :
reconnexion agent → inputs fonctionnels, plus aucun WARN
`lecture longueur` côté agent.
Preuve : [tag v0.7.1-etape6i-multisession-inputs](https://github.com/Sentinel-Ops/Nidan-v2/releases/tag/v0.7.1-etape6i-multisession-inputs)

- **Étape 7, session du 16-17 juillet 2026 (fait)** : échelons A+B du
  pool dynamique Proxmox complétés. `ProxmoxClient` implémente
  l'authentification par token API avec épinglage TLS SHA-256
  (échelon A : lecture — `get_vm_status`, `list_vms`) et les
  opérations d'écriture (échelon B : `clone_vm`, `set_config`,
  `start_vm`, `stop_vm`, `delete_vm` + `wait_for_task` pour les
  tâches async UPID). Validation cycle complet clone → start → stop
  → delete en 133 s (VM 201 depuis template 116).

- **Étape 7, Phase 0 — Sanctuarisation Git (fait, 24 août 2026)** :
  merge de `feat-pool-dynamique-echelon-A` dans `main`, tag
  `v0.7.1-proxmox-final` posé comme point de restauration permanent
  de la version Proxmox fonctionnelle. Décision d'architecture
  documentée dans `docs/plan-action-migration-kvm.md` : conservation
  du repo unique, introduction d'un trait `VmProvider` avec
  `LibvirtProvider` comme cible primaire et `ProxmoxProvider`
  conservé derrière un feature flag Cargo `proxmox-provider`.
  Contexte du pivot : abandon de Proxmox au profit d'une stack
  KVM/libvirt pure sur Dell R440, avec bénéfice attendu de levée de
  la contrainte mono-session (CIDs vsock dynamiquement contrôlables
  par le broker).

- **Étape 7, Phase 1 — v0.7.3 fix portail + clamp (fait, 24-27 août)** :
  session portail XDG unifiée (`portal_session.rs`), clamp openh264
  dimensions paires, tag `v0.7.3-client-perf`.

- **Étape 7, Phases 2-3 — Trait VmProvider + LibvirtProvider (fait,
  28-29 août 2026)** : extraction du trait `VmProvider` avec 8 méthodes
  async, `StaticProvider` par défaut, `LibvirtProvider` complet
  (clone avec backing file, delete volumes, set_vsock_cid, validation
  préfixe). Feature gates : `provider-proxmox`, `provider-libvirt`,
  `provider-host-agent`. ProxmoxClient conservé derrière feature flag.

- **Étape 7, Phase 4.0 — Durcissement LibvirtProvider (fait, 28 août)** :
  suppression des volumes AVANT undefine (pas d'orphelins), validation
  clone (template arrêté, nom unique uuid4, préfixe forcé), format
  qcow2 explicite dans le XML de volume.

- **Étape 7, Échelon C — CidAllocator + provisionnement dynamique
  (fait, 28 août)** : `CidAllocator` avec plage configurable
  (`cid_start`..`cid_end`), `assign_or_provision()` dans le pool
  (assignation statique + fallback dynamique avec rollback en cas
  d'échec), `DynamicPoolConfig` dans la config TOML. Tag intermédiaire
  `v0.7.3-client-perf`.

- **Étape 7, Échelon D — Destruction post-session (fait, 28 août)** :
  `release()` rendu async, distingue VMs statiques (recyclage →
  Available) et VMs dynamiques (stop + delete + libération CID +
  retrait du pool). Tests unitaires mis à jour.

- **Étape 7, JWT CID + proxy_address (fait, 28-29 août)** :
  `SessionClaims.cid: Option<u32>` dans le JWT du broker ET du proxy,
  `NetworkConfig.proxy_address: Option<String>` retournée au client
  au lieu de l'IP directe de la VM. Le proxy extrait le CID du JWT
  pour le routage multi-VM.

- **Étape 7, Canal 2 complet — C2-A/B/C (fait, 29 août)** :
  * C2-A : protocole `AgentRequest`/`AgentResponse`/`AgentVm` dans
    `nidan-proto/src/host_agent.rs` (7 actions, JSON tagué, 10 tests).
  * C2-B : nouveau crate `nidan-host-agent` (binaire Rust, vsock
    listener port 6900, filtrage CID, dispatch libvirt, validation
    préfixe double — handler + lookup_and_verify).
  * C2-C : `HostAgentProvider` dans le broker (impl VmProvider via
    vsock, framing length-prefix JSON, feature `provider-host-agent`).
  * Configuration : `[provider] backend = "host-agent"` +
    `[provider.host_agent] host_cid, port, vm_prefix`.

- **Étape 7, Multi-VM — routage frames + inputs par CID (fait,
  29-30 août)** :
  * `RawFrame.source_cid: Option<u32>` — chaque frame taguée avec le
    CID de l'agent source.
  * `VsockCapturer` multi-agent — `tokio::spawn` par connexion agent
    (au lieu de traitement séquentiel mono-agent).
  * `subscribe_frames_as_mpsc(shutdown, filter_cid)` — filtrage
    optionnel par CID dans le VsockService.
  * Canaux d'inputs per-CID — `register_input_for_cid(cid)` crée un
    canal dédié avant `wait_for_agent`, l'agent récupère le receiver
    à la connexion. `get_input_tx_for_cid(cid)` pour le lookup dans
    `make_injector`. Résout le problème de timing (deux
    `wait_for_agent_capabilities` dans le code).
  * Testé : 3 clients simultanés, chacun sa VM, vidéo + clavier +
    souris isolés.

- **Étape 7, Thin clone qcow2 (fait, 30 août)** : `clone_volumes`
  modifié pour utiliser `qemu-img create -b` (backing file) au lieu de
  `StorageVol::create_xml_from` (copie complète). Clone instantané
  (~0s) au lieu de ~50s pour 16 Go sur HDD. Résout la saturation I/O
  qui mettait les VMs en pause avec 3+ clones simultanés.

- **Étape 7, Fix release prématuré (fait, 30 août)** : suppression du
  `pool.release()` dans le routing handler du broker (la connexion
  broker est un handshake court, le client se déconnecte pour aller au
  proxy). Ajout d'un GC (`gc_expired_sessions`) toutes les 30s qui
  libère les VMs dont le JWT a expiré (`session_token_ttl_secs`).
  Filet de sécurité en cas d'échec du cleanup proxy.

- **Étape 7, Destruction immédiate à la déconnexion (fait, 30 août)** :
  le proxy-encoder détecte `conn.closed()` et envoie `stop_vm` +
  `delete_vm` au host-agent via vsock loopback (CID 1, module
  `vsock_loopback`). Le host-agent accepte le proxy (config
  `proxy_cid = 1`) mais le restreint à stop/delete uniquement — toute
  autre opération est rejetée avec log d'audit. La VM est détruite en
  < 1s après la fermeture du client.

- **Étape 7, Tests end-to-end validés (30 août 2026)** :
  * 3 clients simultanés, 3 VMs dynamiques clonées et démarrées
  * Chaque client voit et contrôle uniquement sa VM (isolation CID)
  * Destruction immédiate à la déconnexion
  * Flux H.264 1280×720, E2E crypto (X25519 + ChaCha20-Poly1305)
  * Thin clone instantané sur HDD SATA

- **Étape 7, v0.8.0-dynamic-pool (fait, 30 août 2026)** :
  commit, tag `v0.8.0-dynamic-pool`, release GitHub. Pool dynamique
  multi-VM fonctionnel sur KVM/libvirt avec host-agent vsock, thin
  clones, destruction immédiate à la déconnexion.

- **Étape 7, Échelon E — Pool chaud (fait, 1-4 septembre 2026)** :
  pré-provisionnement de `min_ready` VMs au boot du broker. Le client
  reçoit une VM déjà démarrée au lieu d'attendre un clone à froid
  (~40 s → < 1 s). Réapprovisionnement automatique en arrière-plan
  après chaque assignation ou libération.
  * `VmState::WarmReady { since }` dans le pool
  * `provision_warm_vm()`, `replenish()`, `replenish_loop()`
  * `assign_or_provision()` cherche WarmReady avant clone à froid
  * Config TOML : `min_ready`, `max_total`, `boot_timeout_secs`

- **Étape 7, Échelon F — GC orphelines avec délai de grâce (fait,
  1-4 septembre)** : boucle périodique (`gc_orphan_loop`) qui détecte
  les VMs dynamiques présentes sur l'hyperviseur mais absentes du pool
  (crash broker, `delete_vm` raté). Délai de grâce configurable pour
  éviter de détruire une VM en cours de provisionnement. Au boot, le
  GC s'exécute immédiatement sans grâce (`skip_grace`) pour nettoyer
  les restes d'un crash précédent.
  * Config TOML : `gc_orphan_interval_secs`, `orphan_grace_secs`
  * `protected_vms: Vec<String>` — liste de VMs à ne jamais détruire
    (obligatoire en production : broker, target, infra)
  * Bug critique corrigé : le GC détruisait la VM broker elle-même
    (préfixe `nidan-` matché mais pas dans le pool DashMap)
  * Fix boot race : `cleanup_orphans(skip_grace=true)` avant
    `replenish()` pour éviter les conflits de CID vsock

- **Étape 7, Échelon G — Quotas par utilisateur (fait,
  1-4 septembre)** : deux niveaux de quota vérifiés avant chaque
  assignation dynamique. Le quota global est vérifié uniquement sur
  les clones à froid (assigner une WarmReady ne crée aucune ressource).
  * `max_per_user` dans `DynamicPoolConfig` (défaut : 3)
  * `user_id: Option<String>` dans `VmPoolEntry`
  * `user_dynamic_count()` par CN du certificat mTLS
  * Quota global déplacé après le check WarmReady

- **Étape 7, Fix input routing proxy-encoder (fait, 2 septembre)** :
  race condition causée par le pool chaud — les agents des VMs
  WarmReady se connectaient au proxy-encoder avant qu'un client ne
  s'enregistre pour leur CID. L'agent tombait sur le canal partagé
  (fallback), puis le client créait un nouveau canal dédié que
  personne ne lisait.
  * Auto-création du canal dédié quand l'agent connecte sans canal
  * `register_input_for_cid()` réutilise le canal existant
  * Clavier/souris fonctionnels sur toutes les sessions (A, B, C)

- **Étape 7, Fix fermeture fenêtre SDL2 (fait, 4 septembre)** :
  `SDL_Quit()` bloque indéfiniment dans les destructeurs sur Linux.
  Le `CancellationToken` depuis un `std::thread` ne réveillait pas
  le waker tokio. Après la sortie de la boucle SDL, un thread envoie
  SIGINT au process après 100 ms, déclenchant le même chemin que
  Ctrl+C : `conn.close()` → le broker libère la VM. Fallback
  `exit(0)` après 500 ms.

- **Étape 7, v0.9.0-warm-pool (fait, 4 septembre 2026)** :
  tag `v0.9.0-warm-pool`, release GitHub. 15 fichiers modifiés,
  +630 / −50 lignes. Séquence de démarrage validée :
  `cleanup_orphans(skip_grace)` → `replenish()` → écoute QUIC.
  3 clients simultanés, assignation < 1 s, destruction automatique
  à la fermeture de la fenêtre.

---

## Étape 8 — Industrialisation

### Objectif

Rendre NIDAN v2 déployable en production par un opérateur tiers, avec
un template VM optimisé, des services système, et une documentation
de déploiement complète pour l'infrastructure KVM/libvirt.

### 8.1 — Optimisation template VM

Le template actuel consomme ~20 Go de RAM et met ~40 s à booter.
Deux axes de réduction :

**RAM (20 Go → 2-4 Go) :**

- Retirer les paquets desktop inutiles (LibreOffice, jeux, snap)
- Configurer `zram-generator` (swap compressé en RAM)
- Tuning kernel : `vm.swappiness=60`, hugepages désactivées
- Audit `systemd-analyze blame` → désactiver les services non
  nécessaires (cups, avahi, ModemManager, unattended-upgrades)

**Boot (40 s → 10-15 s) :**

- Cloud-init en mode `NoCloud` (datasource locale, pas de réseau)
- Supprimer `cloud-init` au profit d'un script firstboot minimal
  (hostname, resize fs, IP statique via pattern CID)
- `systemd-analyze critical-chain` → paralléliser les services
- Kernel cmd : `quiet splash=0 loglevel=3`

**Livrable :**

- Script `prepare-template.sh` (sysprep + optimisation)
- Documentation du processus dans `docs/TEMPLATE-PREPARATION.md`
- Template qcow2 résultant testé (boot < 15 s, RAM < 4 Go)

### 8.2 — Services systemd

Trois services à créer pour le socle KVM :

| Service | Cible | Particularité |
|---------|-------|---------------|
| `nidan-host-agent.service` | socle (root) | accès libvirt, vsock :6900 |
| `nidan-proxy-encoder.service` | socle (user) | QUIC :7610, vsock capture |
| `nidan-broker.service` | VM broker | QUIC :7611, mTLS |

Chaque service inclut : `Restart=on-failure`, `RestartSec=5`,
`LimitNOFILE=65535`, logging vers journald, dépendances ordonnées
(`After=`, `Wants=`).

**Ordre de démarrage garanti :**

```
host-agent.service → proxy-encoder.service → nidan-broker (VM)
```

Le host-agent doit être prêt AVANT le broker (sinon le replenish
échoue — bug découvert en session du 1er septembre).

**Livrable :**

- 3 fichiers `.service` dans `deploy/systemd/`
- Script `deploy/install-services.sh`
- `docs/DEPLOIEMENT-KVM.md` (guide complet)

### 8.3 — Documentation déploiement KVM/libvirt

Guide de déploiement pour l'infrastructure cible (Dell R440 ou
équivalent), couvrant :

- Prérequis matériel et logiciel (Debian 13, libvirt, qemu-kvm)
- Configuration réseau (bridge, NAT, IP statique par CID)
- Configuration PKI (CA, certificats broker/proxy/client)
- Préparation du template VM (renvoi vers 8.1)
- Installation des services (renvoi vers 8.2)
- Configuration TOML de production (protected_vms, quotas, GC)
- Procédure de test de validation (renvoi vers TEST-echelons-E-F-G.md)
- Dépannage (logs, diagnostic vsock, virsh)

**Livrable :**

- `docs/DEPLOIEMENT-KVM.md`
- `docs/TROUBLESHOOTING.md`

### 8.4 — Release v1.0.0

Critères de passage :

- Template optimisé validé (boot < 15 s, RAM < 4 Go)
- Services systemd fonctionnels avec restart automatique
- Documentation déploiement complète et testée par un tiers
- 3 sessions simultanées stables sur 30+ minutes
- Pas de fuite mémoire (vérification `heaptrack` ou `valgrind`)

**Tag :** `v1.0.0`

### Commits prévus

```
chore(template): script prepare-template.sh + doc préparation
feat(deploy): services systemd host-agent, proxy-encoder, broker
docs: guide déploiement KVM/libvirt complet
docs: guide de dépannage
chore: release v1.0.0
```

---

## Étape 9 — Innovation thèse IDPE (C2-E)

### Objectif

Implémenter les briques d'innovation de la thèse IDPE qui
différencient NIDAN des solutions existantes (Sanzu, Guacamole,
RDP/VNC). Ces briques sont aussi le cœur de la valeur CSPN.

### Contexte thèse

Le mémoire IDPE (CNAM/EiCNAM, spécialité Télécom et Réseaux)
porte sur la sécurité des architectures VoWiFi 4G/5G. NIDAN est le
démonstrateur pratique : un système de bureau distant durci qui
applique des principes de sécurité réseau télécom à l'isolation
des postes de travail sensibles.

### 9.1 — Firewall sémantique XML (C2-D)

Le host-agent valide déjà les requêtes par action (clone, start,
stop, delete) et par préfixe de VM. L'étape suivante est la
**validation sémantique du contenu XML libvirt** :

- Whitelist de devices autorisés dans le XML de domaine (virtio-net,
  virtio-gpu, vsock — pas de USB passthrough, pas de PCI passthrough)
- Validation des chemins de stockage (pool `images` uniquement)
- Validation des plages CID vsock (cid_start..cid_end)
- Rejet des configurations dangereuses (`<hostdev>`, `<shmem>`,
  `<filesystem>` en mode passthrough)

**Architecture :**

```
Broker (VM CID 3)
  │
  │ vsock :6900 — AgentRequest (JSON)
  ▼
Host-Agent (socle)
  ├─ Filtrage action (existant)
  ├─ Filtrage préfixe VM (existant)
  ├─ Validation XML sémantique    ← NOUVEAU
  │    ├─ Parse XML avec roxmltree ou quick-xml
  │    ├─ Whitelist devices
  │    ├─ Whitelist chemins stockage
  │    └─ Validation plage CID
  └─ Dispatch libvirt (existant)
```

**Livrable :**

- Module `nidan-host-agent/src/xml_firewall.rs`
- Tests unitaires avec XML valides et invalides
- Documentation du modèle de menace dans `docs/SECURITY-MODEL.md`

### 9.2 — Capacités CID-bound (C2-E.1)

Jetons de capacité cryptographiques liés au CID vsock de l'émetteur.
Le broker émet un jeton signé qui autorise une opération spécifique
sur une VM spécifique, et le host-agent vérifie que le CID de
l'appelant correspond au CID déclaré dans le jeton.

**Principe :**

```
Broker :
  capability = sign(HMAC-SHA256, {
    action: "start_vm",
    target: "nidan-abc123",
    issuer_cid: 3,
    expires: now + 60s,
    nonce: random
  })

Host-Agent :
  1. Vérifier signature HMAC
  2. Vérifier expires > now
  3. Vérifier issuer_cid == peer_cid (vsock SO_PEERCRED)
  4. Vérifier action == requête
  5. Vérifier target == VM cible
  → OK : exécuter
  → KO : rejet + log d'audit
```

**Protection contre :**

- Replay depuis un autre CID (VM compromise)
- Escalade de privilèges (action non autorisée)
- Détournement temporel (jeton expiré)

**Livrable :**

- Module `nidan-host-agent/src/capability.rs`
- Intégration dans le protocole `AgentRequest` (champ `capability_token`)
- Clé partagée HMAC configurée dans le TOML
- Tests unitaires (jeton valide, expiré, CID incorrect, action incorrecte)

### 9.3 — Attestation mutuelle IMA/EVM (C2-E.2)

Intégrité mesurée (IMA) et signée (EVM) pour garantir que le
host-agent et le broker n'ont pas été modifiés. Au démarrage, chaque
composant présente ses mesures à l'autre via vsock.

**Flux :**

```
Boot socle :
  1. IMA mesure les binaires (nidan-host-agent, nidan-proxy-encoder)
  2. EVM signe les mesures avec la clé TPM/certificat
  3. Host-agent démarre, lit /sys/kernel/security/ima/ascii_runtime_measurements

Boot broker VM :
  4. IMA mesure nidan-broker
  5. Au handshake vsock, broker envoie ses mesures au host-agent
  6. Host-agent envoie ses mesures au broker
  7. Chacun vérifie la signature EVM de l'autre
  → Si mismatch : refus de communiquer + alerte
```

**Prérequis :**

- Kernel compilé avec `CONFIG_IMA=y`, `CONFIG_EVM=y`
- Policy IMA configurée (appraise + measure)
- Certificats EVM provisionnés (TPM ou fichier)

**Livrable :**

- Module `nidan-common/src/attestation.rs` (lecture IMA, vérification)
- Intégration dans le handshake host-agent ↔ broker
- Documentation dans `docs/IMA-EVM-SETUP.md`
- Ce module n'est PAS requis pour le fonctionnement normal — il
  s'active par configuration (`[security] ima_attestation = true`)

### 9.4 — Dossier CSPN

La CSPN (Certification de Sécurité de Premier Niveau, ANSSI) est la
cible finale. Les briques 9.1-9.3 renforcent le TOE (Target of
Evaluation) :

| Brique | Apport CSPN |
|--------|-------------|
| Firewall XML | Réduction de la surface d'attaque hyperviseur |
| Capacités CID | Authentification inter-composants sans réseau IP |
| IMA/EVM | Intégrité du code en exécution |

**Livrable :**

- Cible de sécurité CSPN mise à jour (`docs/CSPN/`)
- Matrice de couverture des exigences vs implémentation
- Rapport de test de conformité

### Commits prévus

```
feat(host-agent): firewall sémantique XML libvirt
feat(host-agent): capacités CID-bound (jetons signés HMAC)
feat(common): attestation mutuelle IMA/EVM
docs: modèle de sécurité NIDAN
docs: guide configuration IMA/EVM
docs: cible de sécurité CSPN v2
```

---

## Étape 10 — Fonctionnel avancé

### Objectif

Étendre NIDAN au-delà du MVP pour couvrir les cas d'usage de
production en environnement entreprise/OIV.

### 10.1 — Authentification OIDC / MFA

Le broker supporte déjà mTLS. Ajouter OIDC comme méthode
d'authentification alternative ou complémentaire :

**Architecture :**

```
Client → Broker
  ├─ mTLS (certificat machine — prouve le poste)
  └─ OIDC (JWT Keycloak/Authentik — prouve l'utilisateur + MFA)

Broker vérifie :
  1. Certificat client valide (TLS layer)
  2. JWT OIDC valide (signature, issuer, audience, exp)
  3. Claim "sub" ou "email" → user_id pour les quotas
```

**Avantage :** l'identité vient de l'IdP (facile à gérer, MFA,
SSO), le canal est protégé par mTLS (cert machine). Quotas basés
sur l'identité OIDC.

`OidcConfig` existe déjà dans la config du broker — il faut
l'implémenter.

**Livrable :**

- Module `nidan-broker/src/auth/oidc.rs`
- Intégration Keycloak / Authentik documentée
- Tests avec JWT signé RS256

### 10.2 — PKI automatisée (EST/SCEP)

Pour le déploiement en entreprise (50+ utilisateurs), la gestion
manuelle des certificats ne scale pas. Intégrer un flux
d'enrollment automatique :

**Modèle EST (RFC 7030) :**

```
1. Admin crée le compte dans l'annuaire (LDAP/AD)
2. Utilisateur reçoit un one-time token
3. Le client NIDAN fait un enrollment :
   POST /.well-known/est/simpleenroll
   → reçoit son certificat signé par la CA NIDAN
4. Renouvellement automatique (simplereenroll)
5. Révocation via CRL/OCSP
```

**Livrable :**

- Client EST dans `nidan-client` (enrollment + renouvellement)
- Documentation d'intégration avec `step-ca` ou DogTag
- Flux de révocation OCSP dans le broker

### 10.3 — Multi-nœud (scalabilité horizontale)

L'architecture actuelle est mono-socle. Pour un déploiement
production avec plus de VMs que ce qu'un seul hôte peut supporter :

**Phase 1 — multi-host-agent :**

```
Broker (VM)
  ├─ vsock → host-agent-1 (socle 1, CID 2)
  ├─ vsock → host-agent-2 (socle 2, CID ?)  ← problème : vsock = même hôte
  └─ TLS  → host-agent-2 (réseau IP)        ← solution : transport réseau
```

Le trait `VmProvider` supporte déjà l'abstraction. Il faut ajouter
un transport réseau (mTLS) en plus du vsock pour les agents
distants, et un mécanisme de sélection d'hôte (round-robin, charge,
affinité).

**Phase 2 — broker distribué :**

- Plusieurs brokers derrière un load-balancer
- État partagé (Redis, etcd, ou réplication CRDTs)
- Pas prioritaire — le mono-broker couvre la plupart des cas OIV

**Livrable :**

- `HostAgentProvider` avec transport TLS pour agents distants
- Stratégie de placement dans `VmPool`
- Documentation architecture multi-nœud

### 10.4 — Presse-papier bidirectionnel sécurisé

Le presse-papier client → serveur existe (implémenté en étape 6).
Le serveur → client existe aussi (canal de contrôle QUIC). Renforcer
la sécurité :

- Filtrage MIME (texte uniquement, pas d'images/fichiers par défaut)
- Taille maximale configurable
- Journalisation de chaque transfert (compliance OIV)
- Policy configurable par utilisateur (TOML ou LDAP)

### 10.5 — Audio

Non implémenté actuellement. Le protocole le supporte
(`audio_enabled` dans le handshake). Ajout d'un flux audio
PipeWire → Opus → QUIC stream dédié.

**Priorité :** basse — la plupart des cas d'usage OIV/défense
n'ont pas besoin d'audio sur le bureau distant.

### Commits prévus

```
feat(broker): authentification OIDC / MFA
feat(client): enrollment PKI automatique (EST)
feat(broker): révocation OCSP temps réel
feat(broker): multi-nœud — transport TLS pour host-agents distants
feat(proxy): presse-papier — filtrage MIME + journalisation
feat(proxy+agent): audio PipeWire → Opus → QUIC
```

---

## Priorités et calendrier prévisionnel

```
Septembre 2026 :
  ├─ 8.1 Optimisation template VM
  ├─ 8.2 Services systemd
  └─ 8.3 Documentation déploiement KVM

Octobre 2026 :
  ├─ 9.1 Firewall sémantique XML
  ├─ 9.2 Capacités CID-bound
  └─ 8.4 Release v1.0.0

Novembre 2026 :
  ├─ 9.3 Attestation IMA/EVM
  ├─ 9.4 Dossier CSPN
  └─ 10.1 OIDC / MFA

Décembre 2026+ :
  ├─ 10.2 PKI automatisée
  ├─ 10.3 Multi-nœud
  └─ 10.4-10.5 Presse-papier sécurisé, audio
```

Les étapes 9.x (thèse IDPE) et 8.x (industrialisation) avancent en
parallèle. Les étapes 10.x sont planifiées mais non bloquantes pour
la soutenance ou la CSPN.
