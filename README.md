# NIDAN (v2) — Network Isolated Desktop Access Node -- Sécurisé avec isolation par hyperviseur
 
> Successeur de [Sentinel-Ops/Nidan](https://github.com/Sentinel-Ops/Nidan).
> v2 = refonte pour le modèle de menace **navigateur en quarantaine** :
> l'utilisateur navigue sur Internet depuis une VM potentiellement compromise,
> sans que le poste client puisse être atteint.

## Pourquoi une v2

La v1 fait fonctionner le déport de bureau (Wayland, QUIC, chiffrement E2E
ChaCha20-Poly1305, mTLS, portails XDG pour capture et injection). Elle
protège **le transport** entre le client et le serveur.

Elle **ne protège pas** le poste client contre une VM compromise : le serveur
tourne dans la VM et y produit lui-même le flux H.264. Si un attaquant prend
le contrôle de la VM (par le navigateur, un site piégé, etc.), il peut forger
un flux vidéo malveillant qui exploite le décodeur H.264 du client — le poste
utilisateur devient à son tour compromis.

La v2 corrige ça en s'inspirant du modèle **Sanzu** (CEA-SEC, SSTIC 2022,
*Mise en quarantaine du navigateur*), avec un ajout de sécurité : **le
chiffrement de bout en bout** entre le client et le proxy est conservé.

## Principe : sortir l'encodeur de la VM, isoler la VM par vsock

L'idée centrale est que **la VM ne produit plus de H.264**. Elle envoie
uniquement des **pixels bruts** (RGBA, non parsés) au reste du système, via
un canal **vsock** — un bus virtio point-à-point entre l'hyperviseur et la
VM, sans IP ni routage.

L'encodeur H.264 (et le service exposé au client) sont déplacés sur l'**hôte**,
dans une zone de confiance. Une VM compromise ne peut donc pas fabriquer un
flux vidéo piégé, parce qu'elle n'a jamais accès à l'encodeur : elle ne
produit que des pixels, l'hôte les compresse.

### Le rôle de vsock

`vsock` (`AF_VSOCK` / virtio) est fourni par l'hyperviseur (KVM/QEMU, donc
Proxmox nativement). C'est un canal de communication **hôte ↔ invité** qui
ne passe pas par la pile réseau : pas d'IP, pas de routage, pas de carte
réseau côté VM. L'adressage est un simple couple `(CID, port)` où le CID
identifie la machine (`2` = hôte, chaque VM a un CID unique).

Conséquence pratique : la VM peut être configurée **sans aucune interface
réseau vers le poste client**. Elle ne joint le monde extérieur que par
deux canaux distincts et cloisonnés :

1. **Une interface IP** dédiée à Internet, cantonnée par le pare-feu
   Proxmox (VM → proxy web → Internet ; jamais vers le LAN).
2. **Un canal vsock** vers l'hôte, qui transporte uniquement des pixels
   bruts et des évènements clavier/souris.

Un attaquant qui compromet totalement la VM (navigateur + OS + agent) est
face à ces deux canaux — et rien d'autre. Il ne peut pas scanner ton LAN, ne
peut pas atteindre le client par le réseau, et sur vsock il ne peut envoyer
que des pixels bruts (surface d'attaque réduite à un `bytes` non parsé).

### Ce qui protège le décodeur du client

Le décodeur H.264 du client n'est plus exposé à une entité hostile :
- côté client, le flux H.264 vient du **proxy-encoder** sur l'hôte de
  confiance, jamais directement de la VM ;
- une VM compromise ne peut pas forger de flux piégé, parce qu'elle n'a
  pas d'encodeur H.264 sous la main.

C'est la même propriété que revendique Sanzu (SSTIC 2022, page 6-7) :
« le décodeur vidéo du client est sorti de la surface d'attaque ».

## Architecture

```
                                              HÔTE (Proxmox, zone de confiance)
                                             ┌──────────────────────────────────────────┐
┌──────────────┐                              │                                          │
│    CLIENT    │  ①  QUIC + mTLS + E2E        │  ┌───────────────────────┐               │
│              │ ────────────────────────────►│  │  nidan-proxy-encoder  │               │
│ nidan-client │                              │  │  ─────────────────    │               │
│              │  H.264 chiffré               │  │  reçoit pixels bruts  │               │
│ décodeur     │◄─────────────────────────────│  │  encode H.264         │               │
│ H.264 + SDL  │                              │  │  chiffre E2E ChaCha20 │               │
└──────────────┘                              │  │  expose QUIC au client│               │
   protégé :                                  │  └───────────┬───────────┘               │
   n'est jamais                               │              │ vsock                     │
   en contact                                 │              │ (CID/port)                │
   direct avec la VM                          │              │                           │
                                              │  ┌───────────▼───────────────────────┐   │
                                              │  │ VM Ubuntu (zone hostile potentielle)  │
                                              │  │  ─────────────────────────────    │   │
                                              │  │  nidan-agent (allégé)             │   │
                                              │  │   • capture Wayland (PipeWire)    │   │
                                              │  │   • envoie pixels bruts (RGBA)    │   │
                                              │  │   • injection entrées (RemoteDesktop)│
                                              │  │                                   │   │
                                              │  │  navigateur web ──► Internet      │   │
                                              │  │  (via proxy web, jamais LAN)      │   │
                                              │  └───────────────────────────────────┘   │
                                              └──────────────────────────────────────────┘
```

## Composants

| Composant | Où il tourne | Rôle |
|---|---|---|
| `nidan-client` | poste utilisateur | Décode le H.264, affiche (SDL2), capture clavier/souris |
| `nidan-proxy-encoder` | **hôte Proxmox** (nouveau) | Reçoit les pixels bruts par vsock, encode en H.264, chiffre E2E, expose le service QUIC/mTLS |
| `nidan-agent` | **VM Ubuntu** (allégé) | Capture Wayland, envoie des pixels bruts sur vsock, injecte les entrées |
| `nidan-broker` | hôte (inchangé) | Authentification mTLS, attribution de VM, jetons de session |
| `nidan-proto` | commun | Format des messages (Protobuf) |
| `nidan-common` | commun | Crypto (X25519, HKDF-SHA256, ChaCha20-Poly1305), config |

### Où tourne le proxy-encoder — précision

`nidan-proxy-encoder` s'installe **sur l'hôte Proxmox lui-même**, comme un
service systemd — **pas dans une VM invitée**. C'est un choix structurant du
modèle Sanzu, pour trois raisons :

1. **Accès natif à vsock.** L'API `AF_VSOCK` du noyau hôte donne un accès
   direct au bus virtio-vsock des VMs invitées. Depuis une VM invitée, ce
   même bus n'est pas accessible : vsock est un canal **hôte↔invité**, pas
   invité↔invité.
2. **L'hyperviseur est déjà la racine de confiance.** Si Proxmox est
   compromis, l'attaquant contrôle toutes les VMs de toute façon. Placer
   le proxy sur Proxmox ne dégrade donc pas le niveau de sécurité : il
   est cohérent d'y placer le point de terminaison E2E côté hôte.
3. **La barrière est au bon endroit.** La menace vient de la VM navigateur ;
   la zone à protéger est le poste client. Le proxy sur Proxmox se trouve
   *juste au-dessus* de la zone à risque et *en dessous* du réseau et du
   client — exactement la bonne position.

Le déploiement type :

- **Hôte Proxmox** : le noyau + Proxmox VE + un service systemd
  `nidan-proxy-encoder`, qui expose QUIC sur l'interface réseau (face
  client) et parle vsock à la VM invitée (face agent).
- **VM invitée Ubuntu** : `nidan-agent` en service utilisateur dans la
  session Wayland ; device virtio-vsock (`vhost-vsock-pci`) avec un
  `guest-cid` unique ; aucune interface réseau vers le client
  (interface IP dédiée à Internet uniquement, cantonnée par le pare-feu
  Proxmox).
- **Poste client** : `nidan-client` (inchangé par rapport à la v1 sur
  le principe).

## Modèle de menace couvert

**Menace principale** : un attaquant compromet la VM via le navigateur (site
piégé, exploit de moteur JS, faille de police, etc.) et cherche à atteindre
le poste client.

**Ce que la v2 bloque**
- **Exploit du décodeur H.264 du client** : la VM ne produit pas de H.264 ;
  le flux reçu par le client est toujours produit par un encodeur sain.
- **Accès réseau de la VM au client** : pas de route IP entre les deux ;
  la VM ne parle qu'à l'hôte, par vsock.
- **Exfiltration vers le LAN** : la VM n'a d'accès IP qu'à Internet via
  le proxy web (règle pare-feu Proxmox).
- **Persistance d'une compromission** : la VM est **jetable** (snapshot
  Proxmox restauré entre les sessions).

**Ce que la v2 ne prétend pas résoudre**
- Une faille du proxy-encoder ou de son parseur Protobuf sur l'hôte
  (surface d'attaque réduite à un `bytes` non parsé + `InputBatch`, mais
  non nulle).
- Une faille dans le canal vsock lui-même (implémentation noyau/QEMU).
- Le contenu affiché à l'utilisateur : un site piégé peut toujours tromper
  visuellement (phishing) — c'est un problème hors périmètre.
- L'exfiltration de données au copier-coller si le presse-papier est
  bidirectionnel (recommandation : unidirectionnel Internet → bureau).

## Améliorations par rapport à Sanzu

Ce projet reprend le cœur du modèle Sanzu (encodeur hors VM, pixels bruts,
VM jetable) et **ajoute** :

- **Chiffrement E2E** (X25519 + ChaCha20-Poly1305) sur le canal
  proxy-encoder ↔ client, là où Sanzu s'arrête à TLS. Un attaquant qui
  compromettrait le TLS du côté client (MITM entreprise, faille TLS)
  ne verrait toujours pas le contenu de session.
- **Transport QUIC** (multiplexage, meilleure latence sur réseau dégradé)
  là où Sanzu utilise TCP.
- **Authentification mTLS + jetons JWT** par le broker, indépendante de
  Kerberos (Sanzu suppose un domaine Windows/Kerberos).

## Pré-requis

- **Hôte** : Linux avec KVM/QEMU (Proxmox recommandé), module `vhost-vsock`
  chargé.
- **VM** : Ubuntu 24.04 (Wayland + portails XDG), device `virtio-vsock`
  configuré, aucune interface réseau vers le poste client.
- **Client** : Debian 12 ou Ubuntu 24.04.

Configuration Proxmox de la VM :
- ajouter un device `vhost-vsock-pci` avec un CID unique ;
- interface réseau restreinte au VLAN Internet uniquement, aucun accès LAN
  (pare-feu Proxmox) ;
- image en lecture seule + disque overlay éphémère (bonne pratique) ;
- snapshot de référence pour restauration après session.

## Statut

**v2 en cours de conception.** Le code v1 fonctionnel reste disponible sur
[Sentinel-Ops/Nidan](https://github.com/Sentinel-Ops/Nidan) (tag
`v1.0-fonctionnelle`).

Chantiers en cours :
1. Format Protobuf du canal vsock (pixels bruts + entrées).
2. Extraction de l'encodeur de `nidan-server` v1 vers `nidan-proxy-encoder`.
3. Allègement de `nidan-server` v1 vers `nidan-agent` (retrait encodeur,
   sortie vsock).
4. Configuration Proxmox de référence (vsock, pare-feu, snapshot).

## Références

- Fabrice Desclaux, Frédéric Vannière, *Mise en quarantaine du navigateur*,
  SSTIC 2022 (CEA-SEC).
- [cea-sec/sanzu](https://github.com/cea-sec/sanzu) — l'implémentation
  originale du modèle.

## Licence

GPL-3.0 — voir [LICENSE](LICENSE).
