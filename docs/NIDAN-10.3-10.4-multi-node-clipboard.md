# NIDAN — Étapes 10.3 & 10.4 : Multi-nœud & Presse-papier sécurisé

**Version :** v3.0
**Date :** 4 septembre 2026
**Auteur :** Jo — Sentinel Ops
**Statut :** Planifié (post v1.0.0)

---

## Table des matières

1. Étape 10.3 — Multi-nœud (scalabilité horizontale)
   - Problématique
   - Contrainte vsock
   - Architecture cible
   - Phase 1 : transport réseau host-agents
   - Phase 2 : routage proxy multi-socle
   - Phase 3 : broker distribué
   - Configuration TOML
   - Sécurité
   - Livrables
2. Étape 10.4 — Presse-papier bidirectionnel sécurisé
   - État actuel
   - Modèle de menace
   - Architecture du policy engine
   - Policies configurables
   - Journalisation
   - Modifications
   - Livrables

---

## 1. Étape 10.3 — Multi-nœud (scalabilité horizontale)

### 1.1 Problématique

L'architecture v0.9.0 est mono-socle : broker, proxy-encoder,
host-agent et toutes les VMs dynamiques tournent sur une seule
machine physique (Dell R440). Le nombre de sessions simultanées
est plafonné par la RAM et le CPU de cet hôte.

Pour un déploiement OIV avec 50+ utilisateurs simultanés, il faut
répartir les VMs sur plusieurs serveurs physiques tout en conservant
un point d'entrée unique (le broker).

### 1.2 Contrainte vsock

Le canal vsock (`AF_VSOCK`) ne fonctionne qu'entre VMs et l'hôte
d'une même machine physique. Le broker (VM CID 3 sur le socle 1)
ne peut pas joindre un host-agent sur un autre serveur via vsock.

```
┌──────────────────────────────────────────┐
│              Socle 1 (Dell R440)         │
│                                          │
│  ┌─────────────┐     vsock CID 2         │
│  │ host-agent-1│◄════════════════╗       │
│  └─────────────┘                 ║       │
│                                  ║       │
│  ┌──────────────────┐   vsock    ║       │
│  │ VM broker (CID 3)│═══════════╝       │
│  └──────────────────┘                    │
│                                          │
│  ┌──────────────────┐                    │
│  │ proxy-encoder-1  │                    │
│  └──────────────────┘                    │
└──────────────────────────────────────────┘

┌──────────────────────────────────────────┐
│              Socle 2 (nouveau serveur)   │
│                                          │
│  ┌─────────────┐                         │
│  │ host-agent-2│   ◄── pas de vsock      │
│  └─────────────┘       depuis le broker  │
│                        sur socle 1       │
│  ┌──────────────────┐                    │
│  │ proxy-encoder-2  │                    │
│  └──────────────────┘                    │
└──────────────────────────────────────────┘
```

**Solution :** ajouter un transport mTLS (réseau IP) en complément
du vsock pour les host-agents distants.

### 1.3 Architecture cible

```
                    ┌─────────────────────┐
                    │       Client        │
                    └─────────┬───────────┘
                              │ QUIC mTLS
                              ▼
                    ┌─────────────────────┐
                    │   VM Broker (CID 3) │
                    │                     │
                    │  ┌───────────────┐  │
                    │  │  MultiHostPool│  │
                    │  │               │  │
                    │  │ placement:    │  │
                    │  │  charge +     │  │
                    │  │  capacité     │  │
                    │  └───┬───────┬───┘  │
                    └──────┼───────┼──────┘
                           │       │
               vsock       │       │     mTLS
               (local)     │       │     (réseau)
                           │       │
              ┌────────────┘       └────────────┐
              ▼                                  ▼
┌──────────────────────┐          ┌──────────────────────┐
│     Socle 1          │          │     Socle 2          │
│                      │          │                      │
│  host-agent-1        │          │  host-agent-2        │
│  (vsock :6900)       │          │  (TLS :6901)         │
│                      │          │                      │
│  proxy-encoder-1     │          │  proxy-encoder-2     │
│  (QUIC :7610)        │          │  (QUIC :7610)        │
│                      │          │                      │
│  ┌────┐ ┌────┐       │          │  ┌────┐ ┌────┐      │
│  │VM-A│ │VM-B│       │          │  │VM-C│ │VM-D│      │
│  └────┘ └────┘       │          │  └────┘ └────┘      │
└──────────────────────┘          └──────────────────────┘
```

### 1.4 Phase 1 — Transport réseau pour host-agents distants

Le trait `VmProvider` découple déjà le broker de l'hyperviseur.
Il faut abstraire le **transport** (vsock ou TLS) dans le
`HostAgentProvider`.

```
┌─────────────────────────────────────────────────────┐
│                 HostAgentProvider                    │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │              Transport Layer                  │  │
│  │                                               │  │
│  │  ┌─────────────────┐  ┌────────────────────┐  │  │
│  │  │  VsockTransport │  │    TlsTransport    │  │  │
│  │  │                 │  │                    │  │  │
│  │  │ CID: 2          │  │ addr: 192.168.8.X  │  │  │
│  │  │ port: 6900      │  │ port: 6901         │  │  │
│  │  │                 │  │ cert: mTLS         │  │  │
│  │  │ connect:        │  │                    │  │  │
│  │  │  VsockStream::  │  │ connect:           │  │  │
│  │  │  connect(cid,   │  │  TlsConnector::    │  │  │
│  │  │  port)          │  │  connect(addr)     │  │  │
│  │  └─────────────────┘  └────────────────────┘  │  │
│  └───────────────────────────────────────────────┘  │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │              Protocol Layer                   │  │
│  │  AgentRequest/AgentResponse (JSON, identique) │  │
│  │  Framing : [u32 len][JSON bytes]              │  │
│  └───────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────┘
```

**Trait transport :**

```rust
#[async_trait]
trait AgentTransport: Send + Sync {
    async fn connect(&self) -> Result<Box<dyn AsyncReadWrite>>;
}

struct VsockTransport { cid: u32, port: u32 }
struct TlsTransport   { addr: SocketAddr, tls_config: Arc<ClientConfig> }
```

Le protocole `AgentRequest`/`AgentResponse` reste identique quel
que soit le transport — seul le « tuyau » change.

**Sélection d'hôte (placement) :**

```
                    Requête client
                         │
                         ▼
              ┌──────────────────┐
              │  MultiHostPool   │
              │                  │
              │  agents:         │
              │  ┌────────────┐  │
              │  │ socle-1    │  │
              │  │ cap: 10    │  │
              │  │ actif: 7   │──┼──► charge = 7/10 = 70%
              │  │ transport: │  │
              │  │  vsock     │  │
              │  └────────────┘  │
              │  ┌────────────┐  │
              │  │ socle-2    │  │
              │  │ cap: 15    │  │
              │  │ actif: 3   │──┼──► charge = 3/15 = 20%  ◄── CHOISI
              │  │ transport: │  │
              │  │  tls       │  │
              │  └────────────┘  │
              └──────────────────┘

              Stratégie : round-robin pondéré par charge
              (nombre VMs actives / capacité déclarée)
```

### 1.5 Phase 2 — Routage proxy multi-socle

Chaque socle a son propre proxy-encoder (vsock local vers ses VMs).
Le broker route le client vers le bon proxy via le JWT.

```
  Client                    Broker                   Socle 2
    │                         │                        │
    │  1. Authenticate        │                        │
    │  ──────────────────►    │                        │
    │                         │  2. Select socle-2     │
    │                         │     (lowest load)      │
    │                         │                        │
    │                         │  3. clone_vm (mTLS)    │
    │                         │  ─────────────────►    │
    │                         │                        │
    │                         │  4. VM started         │
    │                         │  ◄─────────────────    │
    │                         │                        │
    │  5. JWT {               │                        │
    │    proxy_address:       │                        │
    │    "192.168.8.200:7610" │                        │
    │    vm_id: "nidan-xxx"   │                        │
    │    cid: 42              │                        │
    │  }                      │                        │
    │  ◄──────────────────    │                        │
    │                         │                        │
    │  6. QUIC connect to proxy-encoder-2              │
    │  ───────────────────────────────────────────►    │
    │                         │                        │
    │  7. Video + Inputs (E2E encrypted)               │
    │  ◄──────────────────────────────────────────►    │
```

Ce mécanisme fonctionne déjà : le broker retourne `proxy_address`
dans le JWT (`NetworkConfig`). Il suffit que chaque host-agent
déclare l'adresse de son proxy-encoder associé dans sa config.

### 1.6 Phase 3 — Broker distribué (optionnel, post-v1)

```
                 ┌─────────────────┐
                 │  Load Balancer  │
                 │  (HAProxy/TLS)  │
                 └────┬────────┬───┘
                      │        │
              ┌───────┘        └───────┐
              ▼                        ▼
     ┌─────────────┐         ┌─────────────┐
     │  Broker A   │         │  Broker B   │
     │  (actif)    │◄───────►│  (actif)    │
     └──────┬──────┘  Redis  └──────┬──────┘
            │         /etcd         │
            ▼                       ▼
     ┌────────────┐          ┌────────────┐
     │ socle 1-2  │          │ socle 3-4  │
     └────────────┘          └────────────┘

     État partagé :
     - Sessions actives (qui a quelle VM)
     - Quotas globaux (max_total cross-broker)
     - Assignations CID (pas de collision)
```

**Non prioritaire** — un seul broker tient des centaines de sessions
(il ne fait que du handshake + émission JWT, pas de trafic data).

### 1.7 Configuration TOML

```toml
# Remplacement de la section [provider.host_agent] actuelle
# par une liste d'agents

[provider]
backend = "multi-host-agent"
placement_strategy = "lowest-load"   # "round-robin" | "lowest-load" | "affinity"

[[provider.agents]]
name = "socle-local"
transport = "vsock"
host_cid = 2
port = 6900
proxy_address = "192.168.8.199:7610"
capacity = 10
vm_prefix = "nidan-"

[[provider.agents]]
name = "socle-remote-1"
transport = "tls"
address = "192.168.8.200:6901"
ca_cert = "/etc/nidan/pki/ca.crt"
client_cert = "/etc/nidan/pki/broker.crt"
client_key = "/etc/nidan/pki/broker.key"
proxy_address = "192.168.8.200:7610"
capacity = 15
vm_prefix = "nidan-"

[[provider.agents]]
name = "socle-remote-2"
transport = "tls"
address = "192.168.8.201:6901"
ca_cert = "/etc/nidan/pki/ca.crt"
client_cert = "/etc/nidan/pki/broker.crt"
client_key = "/etc/nidan/pki/broker.key"
proxy_address = "192.168.8.201:7610"
capacity = 15
vm_prefix = "nidan-"
```

### 1.8 Sécurité

```
┌─────────────────────────────────────────────────┐
│           Modèle de menace multi-nœud           │
├─────────────────────────────────────────────────┤
│                                                 │
│  Menace 1 : interception réseau broker↔agent    │
│  ─────────────────────────────────────────────  │
│  Transport : mTLS (cert machine, CA privée)     │
│  Le protocole JSON est identique au vsock       │
│  Le TLS ajoute le chiffrement que vsock n'a pas │
│                                                 │
│  Menace 2 : agent distant compromis             │
│  ─────────────────────────────────────────────  │
│  Capacités CID-bound (étape 9.2) :              │
│  le broker signe un jeton lié au CN du cert     │
│  de l'agent, pas au CID vsock (pas de vsock     │
│  sur le réseau). L'agent distant ne peut pas    │
│  exécuter d'actions non autorisées.             │
│                                                 │
│  Menace 3 : collision CID entre socles          │
│  ─────────────────────────────────────────────  │
│  Plages CID disjointes par agent :              │
│  socle-1 : 10-99, socle-2 : 100-199            │
│  Configuré dans le TOML de chaque host-agent.   │
│                                                 │
│  Menace 4 : pivot réseau via proxy              │
│  ─────────────────────────────────────────────  │
│  Chaque proxy-encoder est isolé sur son socle.  │
│  Il ne communique qu'en vsock (local) avec ses  │
│  VMs. Pas de routage IP inter-socle.            │
│                                                 │
└─────────────────────────────────────────────────┘
```

### 1.9 Livrables

| Livrable | Fichier | Phase |
|----------|---------|-------|
| Trait `AgentTransport` | `broker/src/provider/transport.rs` | 1 |
| `TlsTransport` | `broker/src/provider/transport_tls.rs` | 1 |
| `MultiHostPool` | `broker/src/pool/multi_host.rs` | 1 |
| Config multi-agents | `broker/src/config.rs` | 1 |
| Routage JWT `proxy_address` dynamique | `broker/src/routing/mod.rs` | 2 |
| Documentation | `docs/MULTI-NODE.md` | 1 |
| Tests d'intégration | `broker/tests/multi_host.rs` | 1 |

---

## 2. Étape 10.4 — Presse-papier bidirectionnel sécurisé

### 2.1 État actuel

Le presse-papier fonctionne dans les deux sens depuis l'étape 6 :

```
┌──────────────────────────────────────────────────────────────┐
│                  Flux actuel (v0.9.0)                        │
│                                                              │
│  ┌────────┐     QUIC ctrl stream      ┌───────────────────┐ │
│  │ Client │◄══════════════════════════►│  Proxy-Encoder    │ │
│  │        │  CTRL_MSG_CLIPBOARD        │                   │ │
│  │ X11/   │  (ChaCha20-Poly1305)       │  relay vers/depuis│ │
│  │ Wayland│                            │  l'agent VM       │ │
│  └────────┘                            └───────────────────┘ │
│                                                              │
│  Direction client → VM :                                     │
│    Ctrl+C client → X11 sélection → CTRL_MSG_CLIPBOARD        │
│    → proxy → vsock → agent → portail RemoteDesktop           │
│    → presse-papier invité                                    │
│                                                              │
│  Direction VM → client :                                     │
│    Copie dans la VM → agent détecte le changement            │
│    → vsock → proxy → CTRL_MSG_CLIPBOARD → client             │
│    → X11/Wayland presse-papier local                         │
│                                                              │
│  Chiffrement : E2E (ChaCha20-Poly1305)               ✓      │
│  Filtrage contenu :                                   ✗      │
│  Journalisation :                                     ✗      │
│  Politique par utilisateur :                          ✗      │
└──────────────────────────────────────────────────────────────┘
```

### 2.2 Modèle de menace

```
┌─────────────────────────────────────────────────────────────┐
│              Menaces presse-papier non filtré                │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  M1 — Exfiltration de données (VM → client)                 │
│  ────────────────────────────────────────────                │
│  Un utilisateur copie un document classifié dans la VM      │
│  et le colle sur son poste local (non classifié).           │
│  Risque : fuite de données sensibles OIV/LPM.              │
│                                                             │
│  M2 — Injection malveillante (client → VM)                  │
│  ────────────────────────────────────────────                │
│  Un attaquant qui a compromis le poste client envoie        │
│  du contenu malveillant dans la VM via le presse-papier     │
│  (payload encodé, commande shell, script).                  │
│                                                             │
│  M3 — Transfert de binaires déguisés                        │
│  ────────────────────────────────────────────                │
│  Des fichiers exécutables ou archives transitent via le     │
│  presse-papier encodés en base64 ou en tant que "texte".    │
│  Contournement des contrôles de transfert de fichiers.      │
│                                                             │
│  M4 — Absence de traçabilité                                │
│  ────────────────────────────────────────────                │
│  Aucune journalisation des transferts → pas d'audit         │
│  possible en cas d'incident → non conforme LPM/NIS2.       │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### 2.3 Architecture du policy engine

```
┌──────────┐                                      ┌──────────┐
│  Client  │                                      │    VM    │
│          │     QUIC ctrl stream (E2E)            │  (agent) │
│  copie   │──────────────────────────────────────►│          │
│   ou     │◄──────────────────────────────────────│  copie   │
│  colle   │     CTRL_MSG_CLIPBOARD                │   ou     │
└──────────┘                                      │  colle   │
      │                                            └──────────┘
      │                                                  │
      ▼                                                  ▼
┌──────────────────────────────────────────────────────────────┐
│                                                              │
│                    Proxy-Encoder                              │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │                  Policy Engine                         │  │
│  │                                                        │  │
│  │  1. Direction autorisée ?                              │  │
│  │     ┌──────────────────────────────────────────┐       │  │
│  │     │ client_to_server: true/false             │       │  │
│  │     │ server_to_client: true/false             │       │  │
│  │     └──────────────────────────────────────────┘       │  │
│  │         │                                              │  │
│  │         ▼                                              │  │
│  │  2. Type MIME autorisé ?                               │  │
│  │     ┌──────────────────────────────────────────┐       │  │
│  │     │ allowed: [text/plain, text/html]         │       │  │
│  │     │ denied:  [image/*, application/*]        │       │  │
│  │     └──────────────────────────────────────────┘       │  │
│  │         │                                              │  │
│  │         ▼                                              │  │
│  │  3. Taille ≤ max ?                                     │  │
│  │     ┌──────────────────────────────────────────┐       │  │
│  │     │ max_size_bytes: 65536 (64 Ko)            │       │  │
│  │     └──────────────────────────────────────────┘       │  │
│  │         │                                              │  │
│  │         ▼                                              │  │
│  │  4. Journaliser                                        │  │
│  │     ┌──────────────────────────────────────────┐       │  │
│  │     │ event: clipboard_transfer                │       │  │
│  │     │ user, direction, mime, size, sha256       │       │  │
│  │     │ decision: allowed / blocked              │       │  │
│  │     └──────────────────────────────────────────┘       │  │
│  │         │                                              │  │
│  │         ▼                                              │  │
│  │  5. Transférer ou bloquer                              │  │
│  │                                                        │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

### 2.4 Flux de décision

```
     Trame CTRL_MSG_CLIPBOARD reçue
                    │
                    ▼
          ┌─────────────────┐
          │ Direction       │     ┌────────────────────┐
          │ autorisée ?     │─NO─►│ BLOQUER            │
          └────────┬────────┘     │ log: "direction    │
                   │ YES          │  interdite"        │
                   ▼              └────────────────────┘
          ┌─────────────────┐
          │ MIME type        │     ┌────────────────────┐
          │ dans whitelist ? │─NO─►│ BLOQUER            │
          └────────┬────────┘     │ log: "type MIME    │
                   │ YES          │  non autorisé"     │
                   ▼              └────────────────────┘
          ┌─────────────────┐
          │ Taille ≤ max ?  │     ┌────────────────────┐
          │                 │─NO─►│ BLOQUER            │
          └────────┬────────┘     │ log: "taille       │
                   │ YES          │  dépassée"         │
                   ▼              └────────────────────┘
          ┌─────────────────┐
          │ Détection        │     ┌────────────────────┐
          │ binaire/base64 ? │─YES►│ BLOQUER            │
          └────────┬────────┘     │ log: "contenu      │
                   │ NO           │  binaire détecté"  │
                   ▼              └────────────────────┘
          ┌─────────────────┐
          │ TRANSFÉRER       │
          │ + journaliser    │
          │ SHA-256 du       │
          │ contenu          │
          └─────────────────┘
```

### 2.5 Policies configurables

**Policy globale (TOML) :**

```toml
[clipboard]
enabled = true

# Directions autorisées
client_to_server = true       # copier vers la VM
server_to_client = true       # copier depuis la VM

# Types MIME autorisés (whitelist)
allowed_mime = [
    "text/plain",
    "text/html",
    "text/uri-list",
]
# Tout ce qui n'est pas dans la liste est bloqué :
# image/png, application/octet-stream, etc.

# Taille maximale par transfert
max_size_bytes = 65536        # 64 Ko

# Détection heuristique de binaires dans du "texte"
detect_binary = true          # bloque si ratio non-printable > 10%
detect_base64 = true          # bloque si contenu ressemble à du base64 > 1 Ko

# Journalisation (compliance LPM / NIS2)
log_transfers = true          # chaque transfert dans journald
log_content_hash = true       # SHA-256 du contenu (traçabilité)
log_content_preview = false   # NE PAS stocker le contenu (données sensibles)
```

**Policies par groupe utilisateur :**

```toml
# Groupe par défaut (si aucun match)
[clipboard.default]
client_to_server = true
server_to_client = true
max_size_bytes = 65536

# Ingénieurs — accès complet, taille étendue
[[clipboard.policies]]
name = "engineering"
match_users = ["alice", "bob"]       # CN du certificat ou claim OIDC
client_to_server = true
server_to_client = true
max_size_bytes = 262144              # 256 Ko
allowed_mime = ["text/plain", "text/html", "text/uri-list"]

# Opérateurs classifiés — exfiltration interdite
[[clipboard.policies]]
name = "classified"
match_users = ["charlie", "delta-*"]  # wildcard sur le CN
client_to_server = true               # peut coller dans la VM
server_to_client = false              # NE PEUT PAS copier depuis la VM
max_size_bytes = 8192                 # 8 Ko max

# Auditeurs — lecture seule, rien ne rentre
[[clipboard.policies]]
name = "auditors"
match_users = ["audit-*"]
client_to_server = false
server_to_client = true
max_size_bytes = 16384
```

**Résolution des policies :**

```
                Trame clipboard reçue
                        │
                        ▼
             ┌──────────────────┐
             │ Extraire user_id │
             │ du JWT session   │
             └────────┬─────────┘
                      │
                      ▼
             ┌──────────────────┐
             │ Match policies   │
             │ par ordre :      │
             │                  │
             │ 1. engineering ? │──► match "alice" → policy engineering
             │ 2. classified ?  │──► match "delta-*" → policy classified
             │ 3. auditors ?    │──► match "audit-*" → policy auditors
             │ 4. default       │──► fallback
             └──────────────────┘
```

### 2.6 Journalisation

Chaque transfert (autorisé ou bloqué) produit un événement structuré
compatible SIEM (JSON, envoyé vers journald/syslog) :

**Transfert autorisé :**

```
┌───────────────────────────────────────────────────┐
│  event: clipboard_transfer                        │
│  timestamp: 2026-09-04T14:30:00.123Z              │
│  session_id: "abc-123-def"                        │
│  user: "alice"                                    │
│  policy: "engineering"                            │
│  direction: "server_to_client"                    │
│  mime: "text/plain"                               │
│  size_bytes: 1247                                 │
│  content_sha256: "a1b2c3d4e5f6..."                │
│  decision: "allowed"                              │
└───────────────────────────────────────────────────┘
```

**Transfert bloqué :**

```
┌───────────────────────────────────────────────────┐
│  event: clipboard_blocked                         │
│  timestamp: 2026-09-04T14:31:00.456Z              │
│  session_id: "abc-123-def"                        │
│  user: "charlie"                                  │
│  policy: "classified"                             │
│  direction: "server_to_client"                    │
│  mime: "text/plain"                               │
│  size_bytes: 4096                                 │
│  reason: "direction interdite (policy classified)"│
│  decision: "blocked"                              │
└───────────────────────────────────────────────────┘
```

**Intégration SIEM :**

```
┌──────────────┐    journald     ┌─────────────┐
│ Proxy-Encoder│────────────────►│   journald   │
│ (events JSON)│                 └──────┬───────┘
└──────────────┘                        │
                                        │ journal-remote
                                        │ ou filebeat
                                        ▼
                                 ┌─────────────┐
                                 │    SIEM      │
                                 │  (Wazuh /    │
                                 │   Elastic /  │
                                 │   Splunk)    │
                                 └─────────────┘
```

### 2.7 Modifications

```
nidan-proxy-encoder/
  └─ src/
      ├─ clipboard_policy.rs     ← NOUVEAU : moteur de policy
      │   ├─ ClipboardPolicy (struct)
      │   ├─ PolicyEngine::evaluate(&self, direction, mime,
      │   │                         size, user) → Decision
      │   ├─ detect_binary(content) → bool
      │   ├─ detect_base64(content) → bool
      │   └─ tests unitaires (12+)
      │
      ├─ stream/mod.rs           ← MODIFIÉ : intercept CTRL_MSG_CLIPBOARD
      │   └─ handle_clipboard_frame()
      │       ├─ extraire user_id du JWT de session
      │       ├─ appeler PolicyEngine::evaluate()
      │       ├─ si allowed → relay + log
      │       └─ si blocked → drop + log
      │
      └─ config.rs               ← MODIFIÉ : section [clipboard]

nidan-proto/
  └─ src/lib.rs                  ← MODIFIÉ : ClipboardTransferRequest
      └─ ajout champ mime_type: String

docs/
  └─ CLIPBOARD-POLICY.md         ← NOUVEAU : documentation policy
```

### 2.8 Livrables

| Livrable | Fichier | Priorité |
|----------|---------|----------|
| Policy engine | `proxy-encoder/src/clipboard_policy.rs` | Haute |
| Intercept dans le stream | `proxy-encoder/src/stream/mod.rs` | Haute |
| Config TOML clipboard | `proxy-encoder/src/config.rs` | Haute |
| Détection binaire/base64 | `clipboard_policy.rs` | Moyenne |
| Policies par utilisateur | `clipboard_policy.rs` | Moyenne |
| MIME type dans le proto | `nidan-proto/src/lib.rs` | Haute |
| Tests unitaires (12+) | `clipboard_policy.rs` | Haute |
| Documentation | `docs/CLIPBOARD-POLICY.md` | Haute |

---

## Commits prévus

```
feat(proxy): policy engine presse-papier (direction, MIME, taille)
feat(proxy): détection heuristique binaires/base64 dans le clipboard
feat(proxy): policies presse-papier par utilisateur (TOML)
feat(proxy): journalisation clipboard compatible SIEM
feat(proto): champ mime_type dans ClipboardTransferRequest
feat(broker): multi-nœud — trait AgentTransport + TlsTransport
feat(broker): MultiHostPool — placement par charge
docs: architecture multi-nœud
docs: policy presse-papier et configuration
```
