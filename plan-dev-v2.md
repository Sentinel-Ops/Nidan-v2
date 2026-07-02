# NIDAN v2 — Plan de développement

Ce document fige la démarche de refonte de NIDAN v1 vers l'architecture v2
(encodeur hors VM, isolation par vsock). Il sert de référence commune et de
trace des choix.

## Principes directeurs

Trois règles qui structurent toute la démarche :

1. **À chaque étape, quelque chose compile et se teste en isolation.**
   Pas de refactor monolithique de plusieurs jours qui ne compile qu'à la
   fin. Chaque étape produit un livrable partiel utilisable.
2. **Après validation d'une étape, on met à jour le repo Nidan-v2.**
   Chaque étape aboutit à un ou plusieurs commits cohérents avec un
   message décrivant l'intention (`feat:`, `docs:`, `chore:`).
3. **On ne casse pas ce qui marche.** Le code v1 est figé sur
   [Sentinel-Ops/Nidan](https://github.com/Sentinel-Ops/Nidan) au tag
   `v1.0-fonctionnelle`. La v2 démarre de cet état comme base.

## Vue d'ensemble — 6 étapes

| # | Étape | Livrable | Durée estimée |
|---|---|---|---|
| 1 | Cadrer le protocole vsock (Protobuf) | `nidan-proto/proto/agent.proto` | Quelques heures |
| 2 | Prototype vsock isolé (validation canal) | 2 binaires de test | Quelques heures |
| 3 | Créer `nidan-proxy-encoder` avec source factice | Crate qui compile + flux test bout-en-bout | 1 jour |
| 4 | Créer `nidan-agent` (allègement de `nidan-server` v1) | Binaire qui envoie pixels bruts sur vsock | 1 jour |
| 5 | Intégration bout-en-bout | Chaîne complète client ↔ proxy ↔ vsock ↔ agent ↔ VM | 0.5 à 1 jour |
| 6 | Documentation Proxmox + polissage | Guide de déploiement + `.deb` | 0.5 jour |

Estimation totale : **4 à 5 jours de développement effectif**, hors
allers-retours de validation en environnement réel.

---

## Étape 1 — Cadrer le protocole vsock

### Objectif

Figer le format des messages Protobuf qui circulent sur vsock entre
`nidan-agent` (VM) et `nidan-proxy-encoder` (hôte). C'est le contrat sur
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
- pour l'étape 3, on branche une implémentation factice de `FrameSource`
  (dégradé animé, pattern de test).

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

Créer `nidan-agent` qui tournera dans la VM Ubuntu, à partir du code de
`nidan-server` v1 dont on retire tout ce qui n'a plus sa place.

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

## Étape 6 — Documentation Proxmox + polissage

### Objectif

Rendre la solution déployable par un tiers, avec un guide clair.

### Livrables

- `docs/DEPLOIEMENT-PROXMOX.md` : configuration de la VM invitée
  (device vsock, CID, pare-feu, réseau Internet-only, snapshot pour
  VM jetable).
- `docs/INSTALLATION-HOTE.md` : installation de `nidan-proxy-encoder`
  sur l'hôte Proxmox (systemd, config, TLS).
- Paquets `.deb` : `nidan-proxy-encoder`, `nidan-agent`,
  `nidan-client`.
- Scripts utilitaires (dérivés de la v1) : PKI, permissions.

### Critère de validation

Un binôme extérieur pourrait déployer NIDAN v2 sur son propre Proxmox
en suivant les docs, sans avoir à demander.

---

## Contraintes de mon environnement de travail

Il faut que ce soit dit clairement pour éviter les malentendus :

- **Je ne peux pas exécuter QEMU/KVM.** Donc dès qu'il s'agit de tester
  vsock, le déploiement Proxmox, ou l'agent dans une VM, c'est **toi
  qui exécutes**, et on communique les résultats.
- **Je peux compiler tout le code Rust ici** (avec les dépendances
  système appropriées).
- **Je peux tester tout ce qui ne nécessite pas vsock ou Wayland
  réels** (proto, encodeur H.264 sur un flux fabriqué, serveur QUIC
  avec un client simulé).

Chaque étape marque explicitement ce qui est testable ici vs ce qui
demande ton environnement.

---

## Cadence de mise à jour du repo

Après chaque étape validée :

```bash
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
- **Prochaine action** : démarrer l'**Étape 1** — rédiger le
  `agent.proto`.
- **Étape 1 (fait)** : `agent.proto` v2 défini, `prost-build` intégré,
  types Rust générés utilisables (`nidan_proto::agent`).
- **Étape 2 (fait)** : canal vsock validé sur Proxmox.
  - VM guest CID=42, hôte CID=2, port 5000.
  - Test 300 frames RGBA 1920×1080 à 30 fps (~2.5 Go transportés).
  - Débit mesuré : **248.7 MB/s** (théorique : 248.8), 0 perte, 0 hors ordre.
  - Le canal vsock encaisse le débit cible sans accumulation de retard.
  - Note : la latence affichée (~32 ms) par le prototype est un artefact
    de mesure (décalage d'horloge VM↔hôte non compensé), pas la vraie
    latence de transit. Une vraie mesure par round-trip sera faite à
    l'étape 5 (intégration bout-en-bout).
- **Prochaine action** : démarrer l'**Étape 3** — créer `nidan-proxy-encoder`.
