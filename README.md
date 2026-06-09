# NIDAN — Network Isolated Desktop Access Node

> Solution de bureau distant graphique haute sécurité, écrite en Rust.  
> Successeur spirituel de [Sanzu (CEA-SEC)](https://github.com/cea-sec/sanzu) avec transport QUIC, chiffrement E2E, audit forensique et clipboard filtré.

## Architecture

```
┌─────────────┐    QUIC/mTLS    ┌──────────────┐    QUIC    ┌──────────────┐
│ nidan-client│ ──────────────► │ nidan-broker │ ─────────► │ nidan-server │
│  (poste     │                 │  (auth +     │            │  (VM isolée) │
│  utilisateur│ ◄── vidéo E2E ──│   routing)   │ ◄── vidéo ─│  X11/Windows │
└─────────────┘                 └──────────────┘            └──────────────┘
                                       │
                                       ▼
                                ┌──────────────┐
                                │ nidan-audit  │
                                │ (recording + │
                                │  watermark)  │
                                └──────────────┘
```

## Composants

| Crate | Rôle | Phase |
|---|---|---|
| `nidan-proto` | Protocole Protobuf | ✅ Phase 0 |
| `nidan-common` | Types partagés, crypto, config | ✅ Phase 0 |
| `nidan-server` | Capture X11 + encodage H.264/H.265/AV1 | Phase 1 |
| `nidan-client` | Décodage + rendu SDL2/wgpu | Phase 1 |
| `nidan-broker` | Auth + routing + pool VMs | Phase 2 |
| `nidan-audit` | Recording MKV + watermark + Prometheus | Phase 3 |

## Avantages vs Sanzu

| Feature | NIDAN | Sanzu |
|---|---|---|
| Transport | QUIC (quinn) | TCP |
| Chiffrement stream | ChaCha20-Poly1305 E2E | TLS uniquement |
| Auth | mTLS + Kerberos + OIDC/SAML | mTLS + Kerberos |
| Multi-moniteur | ✅ | ❌ |
| Clipboard filtré | ✅ | ❌ |
| Enregistrement sessions | ✅ | ❌ |
| Watermarking forensique | ✅ | ❌ |
| Métriques Prometheus | ✅ | ❌ |

## Build

```bash
# Prérequis : Rust 1.75+, protoc, ffmpeg-dev, libxcb-dev
cargo build --workspace
cargo test --workspace
```

## Licence

GPL-3.0 — Voir [LICENSE](LICENSE)
