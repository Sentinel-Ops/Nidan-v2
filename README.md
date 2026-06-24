# NIDAN — Network Isolated Desktop Access Node

> Solution de bureau distant graphique sécurisé, écrite en Rust.
> Transport QUIC, chiffrement applicatif de bout en bout (ChaCha20-Poly1305),
> authentification mutuelle mTLS et broker d'accès. Inspiré de
> [Sanzu (CEA-SEC)](https://github.com/cea-sec/sanzu), avec une couche de
> chiffrement E2E au-dessus du transport.

## Statut du projet

NIDAN fonctionne **de bout en bout** : un client peut s'authentifier auprès du
broker, se voir attribuer un serveur, et contrôler son bureau à distance avec un
flux vidéo chiffré. La capture **et** l'injection des entrées fonctionnent sous
**Wayland natif** (via les portails XDG), ainsi que sous X11/Xorg.

Le projet reste un prototype de recherche : il n'a pas fait l'objet d'un audit
de sécurité indépendant. Voir la section *Limites connues* avant tout usage en
production.

## Architecture

```
┌──────────────┐   (1) auth mTLS + jeton  ┌──────────────┐
│ nidan-client │ ───────────────────────► │ nidan-broker │
│ (poste user) │ ◄── (2) adresse serveur ─│ (auth +      │
│              │                          │  attribution)│
└──────┬───────┘                          └──────────────┘
       │
       │  (3) session vidéo + entrées + presse-papier
       │      chiffrée E2E (X25519 + ChaCha20-Poly1305) — directe
       ▼
┌──────────────┐
│ nidan-server │  capture écran + injection entrées
│ (Wayland/X11)│  (portails ScreenCast + RemoteDesktop, ou X11/XTEST)
└──────────────┘
```

Dans l'implémentation actuelle, le broker authentifie le client, lui attribue un
serveur depuis un pool statique et délivre un jeton de session ; le flux vidéo
transite **directement** entre le client et le serveur (le broker n'est pas sur
le chemin du flux et n'en voit pas le contenu).

## Composants

| Crate | Rôle |
|---|---|
| `nidan-proto` | Définitions du protocole et types de messages |
| `nidan-common` | Crypto (X25519/HKDF/ChaCha20-Poly1305), config, filtrage presse-papier |
| `nidan-server` | Capture écran + encodage H.264 + injection des entrées |
| `nidan-client` | Décodage H.264 + rendu SDL2 + capture des entrées + presse-papier |
| `nidan-broker` | Authentification mTLS, attribution de VM, jetons de session |
| `nidan-audit` | Démon d'enregistrement + watermark (composant séparé, non intégré au flux) |

## Fonctionnalités

**Implémentées et fonctionnelles :**

- Transport **QUIC** (quinn) avec **mTLS** (authentification mutuelle par certificats).
- **Chiffrement de bout en bout** au niveau applicatif : échange de clés ECDH
  **X25519**, dérivation **HKDF-SHA256**, chiffrement **ChaCha20-Poly1305** —
  appliqué à la vidéo, aux entrées et au presse-papier.
- **Capture d'écran Wayland** via le portail `ScreenCast` (PipeWire), avec
  autorisation utilisateur. Backend **X11** également disponible.
- **Injection des entrées** clavier/souris :
  - sous **Wayland** : via le portail `RemoteDesktop` (mapping scancode→evdev) ;
  - sous **X11/Xorg** : via XTEST ;
  - le backend est choisi automatiquement selon `capture.backend`.
- **Encodage vidéo H.264** (openh264).
- **Courtier d'accès** : pool de serveurs déclaré statiquement, attribution +
  jeton de session signé (JWT), health check par handshake QUIC réel.
- **Presse-papier bidirectionnel** avec **filtrage par motifs** côté serveur
  (ex. blocage des clés privées).
- Arrêt propre sur **Ctrl+C** (client et serveur).

**Partielles ou en chantier :**

- `nidan-audit` (enregistrement de session + watermark stéganographique) existe
  comme démon autonome mais **n'est pas branché** sur le chemin du flux.
- Authentification **OIDC** : présente à l'état de **stub** (ne valide pas encore
  la signature des jetons). Seul **mTLS** est pleinement opérationnel.
- Relais du flux par le broker (isolation réseau client/VM par un proxy) : **non
  implémenté** — le client se connecte directement au serveur.

## Build

Prérequis : Rust stable, et selon les features : `libpipewire-0.3-dev`,
`libdbus-1-dev`, `clang` (Wayland) ; `libxcb`/`x11` (X11) ; `libsdl2-dev`
(client) ; openh264.

```bash
# Serveur — cible Wayland (capture + injection portails)
cargo build -p nidan-server --release \
    --no-default-features --features "pipewire-capture openh264 remotedesktop-input"

# Serveur — cible X11/Xorg (capture XGetImage + injection XTEST)
cargo build -p nidan-server --release \
    --no-default-features --features "x11-capture openh264"

# Client
cargo build -p nidan-client --release \
    --no-default-features --features "sdl2-renderer openh264 x11-clipboard wayland-clipboard"

# Broker
cargo build -p nidan-broker --release
```

> **Important (Wayland) :** le serveur doit être lancé **dans la session
> graphique** de l'utilisateur (pas via SSH nu), car les portails ScreenCast et
> RemoteDesktop sont des services de session. Une unité systemd `--user` est
> fournie à cet effet.

## Déploiement

Le déploiement multi-machines (PKI, configuration par rôle, paquets `.deb`,
phases de test) est décrit dans `GUIDE-DEPLOIEMENT.md`.

Chaîne type :
1. Générer la PKI (`scripts/pki-deploy.sh`) et distribuer les certificats.
2. Lancer le serveur dans sa session graphique (backend `wayland`).
3. Lancer le broker (même secret JWT que le serveur).
4. Connecter le client (directement, ou via le broker).

## Modèle de sécurité — résumé

- Le **chiffrement E2E** protège le contenu de session contre un intermédiaire
  réseau, et — le flux étant direct client↔serveur — le broker ne voit jamais le
  contenu.
- Le **broker** ne participe pas à l'échange de clés client↔serveur ; l'identité
  du serveur est vérifiée par le client via la CA.
- Un **serveur/VM compromis** reste un risque pour le client (décodeur H.264,
  presse-papier) : voir `docs/ANALYSE-SECURITE-VM-vers-Client.md`.

## Limites connues

- Prototype non audité ; pas de certification.
- Pool de serveurs **statique** (pas d'orchestration dynamique de VM).
- OIDC non finalisé (stub) ; seul mTLS est opérationnel.
- `nidan-audit` non intégré au flux.
- Sous Wayland, deux autorisations de portail peuvent être demandées (capture +
  contrôle).

## Licence

GPL-3.0 — voir [LICENSE](LICENSE).
