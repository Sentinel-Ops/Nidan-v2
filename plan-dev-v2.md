# NIDAN v2 — Plan de développement

Ce document fige la démarche de refonte de NIDAN v1 vers l'architecture v2
(encodeur hors VM, isolation par vsock). Il sert de référence commune et de
trace des choix.

## Principes directeurs

Trois règles qui structurent toute la démarche :

1. **À chaque étape, quelque chose compile et se teste en isolation.** Pas de refactor monolithique de plusieurs jours qui ne compile qu'à la
fin. Chaque étape produit un livrable partiel utilisable.
2. **Après validation d'une étape, on met à jour le repo Nidan-v2.** Chaque étape aboutit à un ou plusieurs commits cohérents avec un
message décrivant l'intention (`feat:`, `docs:`, `chore:`).
3. **On ne casse pas ce qui marche.** Le code v1 est figé sur
[Sentinel-Ops/Nidan](https://github.com/Sentinel-Ops/Nidan) au tag
`v1.0-fonctionnelle`. La v2 démarre de cet état comme base.

## Vue d'ensemble — 6 étapes

| # | Étape                                                 | Livrable                                            | Durée estimée   |
| --- | ----------------------------------------------------- | --------------------------------------------------- | --------------- |
| 1 | Cadrer le protocole vsock (Protobuf)                  | `nidan-proto/proto/agent.proto`                     | Quelques heures |
| 2 | Prototype vsock isolé (validation canal)              | 2 binaires de test                                  | Quelques heures |
| 3 | Créer `nidan-proxy-encoder` avec source factice       | Crate qui compile + flux test bout-en-bout          | 1 jour          |
| 4 | Créer `nidan-agent` (allègement de `nidan-server` v1) | Binaire qui envoie pixels bruts sur vsock           | 1 jour          |
| 5 | Intégration bout-en-bout                              | Chaîne complète client ↔ proxy ↔ vsock ↔ agent ↔ VM | 0.5 à 1 jour    |
| 6 | Documentation Proxmox + robustesse + polissage        | Guide de déploiement + `.deb` + multi-session       | 1 à 2 jours     |

Estimation totale initiale : **4 à 5 jours de développement effectif**, hors
allers-retours de validation en environnement réel. L'étape 6 s'est étendue
au-delà de l'estimation initiale suite à la découverte et correction d'un
bug de fond dans le pipeline d'encodage (voir détail plus bas).

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

- **Prochaine action** : blocs 3 (packages `.deb`) et 5 (VM jetable
avec snapshot restauré, mode Sanzu original) — reportés en v2.1/v3,
non bloquants pour un usage courant. Éventuellement : article MISC
Magazine sur la sécurité VoWiFi (en cours, indépendant de NIDAN),
poursuite de la préparation aux entretiens sécurité défense/spatial.

## Footer

[https://github.com](https://github.com) © 2026 GitHub, Inc.
