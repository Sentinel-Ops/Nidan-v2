# NIDAN v2 — Aide-mémoire : variables d'env et commandes de test

Ce document rassemble les variables d'environnement et les commandes de
lancement utiles pour tester les composants NIDAN v1 et v2 en local, en
lab et en pré-production. À garder sous la main pendant les étapes de
développement.

## Variables d'environnement — tous les composants

| Composant                | Variable de config      | Chemin par défaut                   | Version |
| :---                     | :---                    | :---                                | :---:   |
| `nidan-proxy-encoder`    | `NIDAN_PROXY_CONFIG`    | `/etc/nidan-proxy-encoder.toml`     | v2      |
| `nidan-agent`            | `NIDAN_AGENT_CONFIG`    | `/etc/nidan-agent.toml`             | v2      |
| `nidan-server`           | `NIDAN_SERVER_CONFIG`   | `/etc/nidan-server.toml`            | v1      |
| `nidan-broker`           | `-c` / `NIDAN_BROKER_CONFIG`  | `/etc/nidan-broker.toml`      | v1      |
| `nidan-client`           | `-c` / `NIDAN_CLIENT_CONFIG`  | `/etc/nidan-client.toml`      | v1      |

**Variables globales** (tous les composants) :

- `NIDAN_LOG` : niveau de log — `error`, `warn`, `info`, `debug`, `trace`.
  Défaut : `info`.
- `RUST_BACKTRACE=1` : afficher la stack en cas de panic (utile en debug).

**Variables spécifiques v2** :

- `NIDAN_VSOCK_PORT` : surcharge le port vsock d'écoute du `nidan-proxy-encoder`.
  Défaut : `6100`. Utile pour tester des sessions parallèles ou éviter une
  collision.

## Convention des chemins de config

Les binaires cherchent par défaut leur config dans `/etc/<nom>.toml`.
Pour regrouper toutes les configs NIDAN sous un même répertoire, on peut
les placer dans `/etc/nidan/` et surcharger par variable d'env :

```
/etc/nidan/nidan-proxy-encoder.toml
/etc/nidan/nidan-agent.toml
/etc/nidan/nidan-broker.toml
/etc/nidan/nidan-client.toml
/etc/nidan/nidan-server.toml
/etc/nidan/certs/{ca.crt,server.crt,server.key,client.crt,client.key}
```

Cette convention est purement locale (au niveau de tes déploiements), le
code ne l'impose pas.

---

## Commandes de test — composants v2

### 1. `nidan-proxy-encoder` (v2, sur l'hôte Proxmox ou VM cible en dev)

**Test à sec — vérifier que le binaire démarre proprement** :

```bash
NIDAN_LOG=info ./target/debug/nidan-proxy-encoder
# Attendu :
#   INFO NIDAN démarré component="nidan-proxy-encoder" version="0.1.0"
#   INFO nidan-proxy-encoder démarrage config=/etc/nidan-proxy-encoder.toml
#   Error: chargement config: /etc/nidan-proxy-encoder.toml   (attendu si absent)
```

**Lancement avec config dans `/etc/nidan/`** :

```bash
NIDAN_PROXY_CONFIG=/etc/nidan/nidan-proxy-encoder.toml \
    NIDAN_LOG=info \
    ./target/debug/nidan-proxy-encoder
```

**Lancement en mode vsock, port par défaut (6100)** :

```bash
NIDAN_PROXY_CONFIG=/etc/nidan/nidan-proxy-encoder.toml \
    NIDAN_LOG=info \
    ./target/debug/nidan-proxy-encoder
# La config doit contenir : [capture] backend = "vsock"
# Attendu au démarrage :
#   INFO initialisation capturer vsock (écoute côté hôte) port=6100
#   INFO VsockCapturer : écoute vsock côté hôte port=6100
#   INFO serveur QUIC en écoute addr=0.0.0.0:7610
#   INFO serveur NIDAN prêt, en attente de connexions
```

**Lancement en mode vsock, port personnalisé** :

```bash
NIDAN_PROXY_CONFIG=/etc/nidan/nidan-proxy-encoder.toml \
    NIDAN_VSOCK_PORT=7100 \
    NIDAN_LOG=info \
    ./target/debug/nidan-proxy-encoder
# L'agent devra utiliser [vsock] port = 7100 dans sa config.
```

**Lancement en mode stub (validation face client sans agent)** :

```bash
# Config doit contenir : [capture] backend = "stub"
NIDAN_PROXY_CONFIG=/etc/nidan/nidan-proxy-encoder-stub.toml \
    NIDAN_LOG=info \
    ./target/debug/nidan-proxy-encoder
```

**Debug maximum — pour voir passer les frames et le handshake vsock** :

```bash
NIDAN_PROXY_CONFIG=/etc/nidan/nidan-proxy-encoder.toml \
    NIDAN_LOG=debug \
    RUST_BACKTRACE=1 \
    ./target/debug/nidan-proxy-encoder
```

### 2. `nidan-agent` (v2, DANS la VM invitée)

**Test à sec** :

```bash
NIDAN_LOG=info ./nidan-agent
# Attendu :
#   INFO NIDAN démarré component="nidan-agent" version="0.1.0"
#   INFO chargement de la config config=/etc/nidan-agent.toml
#   Error: chargement config (attendu si absent)
```

**Lancement avec config dans `/etc/nidan/`** :

```bash
NIDAN_AGENT_CONFIG=/etc/nidan/nidan-agent.toml \
    NIDAN_LOG=info \
    ./nidan-agent
# Attendu :
#   INFO capturer prêt backend=stub width=1920 height=1080 pixel_format=Bgra8888
#   INFO connexion vsock vers le proxy-encoder host_cid=2 port=6100
#   (si le proxy n'écoute pas encore : Error connexion refused)
#   (si le proxy écoute : handshake OK et envoi de frames)
```

**Lancement en mode wayland réel (nécessite recompilation)** :

```bash
# Recompilation avec la feature wayland (une fois)
cargo build -p nidan-agent --features wayland

# Config doit contenir : [capture] backend = "pipewire"
NIDAN_AGENT_CONFIG=/etc/nidan/nidan-agent.toml \
    NIDAN_LOG=info \
    ./target/debug/nidan-agent
# Le portail ScreenCast va ouvrir une popup au premier lancement pour
# autoriser la capture d'écran (comme en v1).
```

**Debug maximum** :

```bash
NIDAN_AGENT_CONFIG=/etc/nidan/nidan-agent.toml \
    NIDAN_LOG=debug \
    RUST_BACKTRACE=1 \
    ./nidan-agent
```

---

## Commandes de test — composants v1 (rappel)

### `nidan-broker` (v1, machine dédiée broker)

**Lancement standard** :

```bash
NIDAN_LOG=info ./target/debug/nidan-broker -c /etc/nidan/nidan-broker.toml
```

**Debug — voir les décisions d'attribution de VM** :

```bash
NIDAN_LOG=debug ./target/debug/nidan-broker -c /etc/nidan/nidan-broker.toml
```

### `nidan-client` (v1, machine cliente)

**Lancement standard** :

```bash
NIDAN_LOG=info /usr/bin/nidan-client -c /etc/nidan/nidan-client.toml
```

**Debug — voir handshake E2E et décodage** :

```bash
NIDAN_LOG=debug /usr/bin/nidan-client -c /etc/nidan/nidan-client.toml
```

### `nidan-server` (v1, historique)

```bash
NIDAN_SERVER_CONFIG=/etc/nidan/nidan-server.toml \
    NIDAN_LOG=info \
    ./target/debug/nidan-server
```

---

## Scénarios de test bout-en-bout

### Scénario A : test face client (étape 3) — proxy-encoder en mode stub

Le plus simple. Utile pour valider QUIC + mTLS + E2E + encodage sans vsock.

**Sur la VM cible (192.168.8.100)** :

```bash
# Config avec backend = "stub"
NIDAN_PROXY_CONFIG=/etc/nidan/nidan-proxy-encoder-stub.toml \
    NIDAN_LOG=info \
    ./target/debug/nidan-proxy-encoder
```

**Sur le client (Debian 12)** :

```bash
NIDAN_LOG=info /usr/bin/nidan-client -c /etc/nidan/nidan-client.toml
```

**Attendu** : dégradé RVB animé dans la fenêtre SDL2.

### Scénario B : test bout-en-bout stub via vsock (étape 5B)

Le vrai test v2 minimal — les frames traversent vsock pour la première fois.

**Sur l'hôte Proxmox** :

```bash
NIDAN_PROXY_CONFIG=/etc/nidan/nidan-proxy-encoder.toml \
    NIDAN_LOG=info \
    /opt/nidan/nidan-proxy-encoder
# Config avec [capture] backend = "vsock"
```

**Dans la VM cible (192.168.8.100)** :

```bash
NIDAN_AGENT_CONFIG=/etc/nidan/nidan-agent.toml \
    NIDAN_LOG=info \
    ~/nidan-agent
# Config avec [capture] backend = "stub", [vsock] host_cid = 2, port = 6100
```

**Sur le client (Debian 12)** :

```bash
NIDAN_LOG=info /usr/bin/nidan-client -c /etc/nidan/nidan-client.toml
```

**Attendu** : même dégradé RVB, mais transporté par vsock (à confirmer par
les logs côté proxy : `VsockCapturer : agent connecté peer_cid=42`).

### Scénario C : test avec vraie capture Wayland (étape 5C, à venir)

**Sur l'hôte Proxmox** : idem scénario B.

**Dans la VM cible** :

```bash
# Config avec [capture] backend = "pipewire"
NIDAN_AGENT_CONFIG=/etc/nidan/nidan-agent.toml \
    NIDAN_LOG=info \
    ~/nidan-agent
# Le portail ScreenCast va demander l'autorisation au premier lancement.
```

**Attendu** : le vrai bureau Wayland de la VM s'affiche côté client.

---

## Utilitaires de diagnostic

### Vérifier que le binaire compile et démarre

```bash
cargo build -p <crate>          # nidan-proxy-encoder ou nidan-agent
./target/debug/<crate>          # doit afficher au minimum les 2 lignes INFO
```

### Vérifier vsock côté hôte Proxmox

```bash
lsmod | grep vhost_vsock         # doit contenir vhost_vsock
ls -la /dev/vhost-vsock          # doit exister (mode crw-rw---- root:kvm)
qm config <VMID> | grep args     # doit contenir vhost-vsock-pci,guest-cid=XX
ps auxww | grep "qemu.*/<VMID>" | grep vsock
                                 # doit montrer -device vhost-vsock-pci,guest-cid=XX
```

### Vérifier vsock côté invité (VM)

```bash
lsmod | grep vsock               # doit contenir vmw_vsock_virtio_transport
ls -la /dev/vsock                # doit exister (crw-rw-rw-)
lspci -nn | grep -i vsock        # doit montrer [1af4:1053]
```

### Vérifier qu'un seul processus écoute sur un port

```bash
sudo ss -tlnp | grep 7610        # port QUIC du proxy-encoder
sudo ss -tlnp | grep 7611        # port QUIC du broker
```

### Vérifier les chemins de config effectifs

```bash
sudo find /etc -name "nidan-*.toml" -o -name "nidan_*.toml" | sort
```

### Comparer les jwt_secret des composants (doit être identique entre broker et proxy)

```bash
sudo grep jwt_secret /etc/nidan/nidan-broker.toml \
                    /etc/nidan/nidan-proxy-encoder.toml \
                    /etc/nidan/nidan-server.toml 2>/dev/null
```

---

## Pièges courants

**« Error: chargement config » alors que le fichier existe** :
→ vérifier que la bonne variable d'env est utilisée. Le proxy utilise
`NIDAN_PROXY_CONFIG`, l'agent utilise `NIDAN_AGENT_CONFIG` — pas
`NIDAN_SERVER_CONFIG` (piège classique).

**« connection refused » côté agent vers vsock** :
→ le proxy n'écoute pas encore, ou pas sur le bon port. Lancer d'abord
le proxy sur l'hôte, puis l'agent dans la VM.

**« Inappropriate ioctl for device » sur `/dev/vsock`** :
→ le device virtio-vsock n'a pas été passé à la VM. Vérifier côté hôte
avec `qm config <VMID> | grep args` puis `qm stop && qm start` (pas un
simple reboot).

**Le client se déconnecte après ~3 secondes en mode stub** :
→ comportement attendu du StubCapturer (nombre de frames limité). Passer
en mode wayland pour un flux permanent.

**« non-fast-forward » sur `git push`** :
→ intégrer les commits distants d'abord :
```bash
git pull --rebase
git push
```
