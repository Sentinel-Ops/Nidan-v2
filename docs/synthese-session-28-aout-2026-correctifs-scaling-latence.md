# NIDAN v2 — Synthèse : Correctifs pipeline client/encodeur (scaling + latence)

**Date :** 2026-08-28
**Base de code :** v0.7.2 (tag `v0.7.2-etape6j-portal-unifie`)
**Contexte :** Travail intercalé dans la Phase 2 de l'étape 7 (migration Proxmox → KVM/libvirt)

---

## Origine du problème

Lors d'une session de test du bureau distant NIDAN v2, deux problèmes ont été constatés à partir des logs client et d'une capture d'écran du wiki interne :

1. **L'image ne remplit pas la fenêtre SDL2** — le contenu vidéo ne s'adapte pas à la taille réelle de la fenêtre, laissant des bandes noires ou un décalage au premier affichage.

2. **Latence perceptible** — délai entre l'action utilisateur (souris/clavier) et le rendu à l'écran, avec des frames perdues côté proxy-encoder.

Les logs initiaux montraient :

```
handshake OK — démarrage session width=1920 height=1080 codec=1
décodeur démarré hw=true
décodeur H.264 openh264 initialisé      ← contradiction : hw=true mais openh264 = software
decoded=300 dropped=0                    ← 300 frames en ~16 min → ~0.3 fps effectif
```

---

## Analyse du code source

L'intégralité du code source v0.7.2 a été analysée fichier par fichier. Voici les constats :

### Côté client (`nidan-client`)

| Fichier | Problème identifié |
|---|---|
| `renderer/mod.rs:146` | `sync_channel::<DecodedFrame>(4)` hardcodé — 4 frames de buffer entre décodeur et renderer. À 30 fps = +132 ms de latence incompressible. La valeur `config.video.decode_buffer_size` (défaut 4 dans config.rs) n'est jamais câblée. |
| `renderer/sdl.rs:89-91` | `win_w` et `win_h` initialisés à `initial_width`/`initial_height` (= résolution du stream). Si le WM redimensionne la fenêtre ou en HiDPI, le `RenderRect` initial est faux. Pas de `canvas.output_size()`. |
| `renderer/sdl.rs:119-124` | Handler `WindowEvent::Resized` utilise `initial_width`/`initial_height` dans `RenderRect::compute()` — ne tient pas compte des changements de résolution du stream en cours de session. |
| `renderer/sdl.rs:200-203` | `TODO : recréer texture avec nouvelles dims` — non implémenté. Si la VM change de résolution, la texture garde les anciennes dimensions → corruption visuelle. |
| `decoder/mod.rs:117` | Log `hw = self.hardware_decode` affiche la **demande** (true), pas le backend réel (openh264 = software) → trompeur pour le debug. |
| `decoder/openh264_dec.rs:28` | Paramètre `_hardware: bool` ignoré — pas de fallback VA-API implémenté. |
| `renderer/sdl.rs:65-73` | `present_vsync()` désactivé (commentaire étape 6g) — déjà correct. |

### Côté proxy-encoder (`nidan-proxy-encoder`)

| Fichier | Problème identifié |
|---|---|
| `encoder/params.rs:55` | `keyframe_interval_secs: 2` → GOP = fps × 2 = 60 frames. Trop long pour du remote desktop, ralentit la resynchronisation après perte de frames. |
| `nidan-proxy-encoder.toml:27` | `max_fps = 20` — plafonné trop bas pour du bureau fluide. |
| `encoder/openh264_enc.rs:139-153` | Conversion BGRA→RGB pixel par pixel dans une boucle `while` avec bounds checks manuels — CPU-intensive, non vectorisable. |

### Bugs latents identifiés (non bloquants)

| Fichier | Bug |
|---|---|
| `encoder/openh264_enc.rs:109-114` | Commentaire dit "BGRA → RGB" mais le code copie `bgra[i]` directement sans swap — incohérent si PipeWire livre du RGBA (commentaire en ligne le mentionne). |
| `renderer/mod.rs:146` | `config.video.decode_buffer_size` existe dans la config mais n'est jamais lu par `start_renderer()`. |

---

## Corrections appliquées

Un script Python (`nidan-v2-check.py`) a été développé pour automatiser les diagnostics et les corrections. Le script supporte trois modes : `dry-run` (défaut), `--apply` (avec backup SHA-256 + rollback), et `--rollback`.

### 7 checks automatisés

| # | Check | Correction | Fichier |
|---|---|---|---|
| 1 | Buffer `sync_channel(4)` | Réduit à `sync_channel(1)` | `renderer/mod.rs` |
| 2 | `RenderRect` init + suivi résolution stream | Ajout `canvas.output_size()`, déclaration `stream_w`/`stream_h`, fix handler `Resized` | `renderer/sdl.rs` |
| 3 | Recréation de texture | Remplacement du TODO par recréation dynamique de texture + mise à jour `stream_w`/`stream_h` + recalcul `RenderRect` | `renderer/sdl.rs` |
| 4 | Log `hw=` trompeur | Renommé en `hw_requested=` | `decoder/mod.rs` |
| 5 | VA-API | Vérification système (vainfo) + diagnostic code — informatif uniquement | — |
| 6 | GOP + FPS | `keyframe_interval_secs: 2→1`, `max_fps: 20→30` | `encoder/params.rs` + TOML |
| 7 | SDL VSync | Vérification que `present_vsync()` est bien absent | `renderer/sdl.rs` |

### Détail des patches sur `sdl.rs` (les plus structurels)

**Avant :**
```rust
let scaling = ScalingMode::from_str(&config.scaling);
let mut win_w = initial_width;
let mut win_h = initial_height;
let mut render_rect = RenderRect::compute(initial_width, initial_height, win_w, win_h, scaling);
```

**Après :**
```rust
let scaling = ScalingMode::from_str(&config.scaling);

// FIX: recalcul avec la taille réelle de la fenêtre (WM, HiDPI)
let (real_w, real_h) = canvas.output_size()
    .unwrap_or((initial_width, initial_height));
let mut win_w = real_w;
let mut win_h = real_h;
// Résolution courante du stream (peut changer si la VM resize)
let mut stream_w = initial_width;
let mut stream_h = initial_height;
let mut render_rect = RenderRect::compute(stream_w, stream_h, win_w, win_h, scaling);
```

**Handler Resized — avant :**
```rust
render_rect = RenderRect::compute(initial_width, initial_height, win_w, win_h, scaling);
```

**Après :**
```rust
render_rect = RenderRect::compute(stream_w, stream_h, win_w, win_h, scaling);
```

**Bloc texture — avant :**
```rust
if w != initial_width || h != initial_height {
    // TODO : recréer texture avec nouvelles dims
    debug!(w, h, "changement de résolution");
}
```

**Après :**
```rust
if w != stream_w || h != stream_h {
    texture = texture_creator
        .create_texture_streaming(PixelFormatEnum::ABGR8888, w, h)
        .context("recréation texture après changement résolution")?;
    stream_w = w;
    stream_h = h;
    render_rect = RenderRect::compute(stream_w, stream_h, win_w, win_h, scaling);
    info!(stream_w, stream_h, "texture recréée pour nouvelle résolution");
}
```

### Pourquoi PAS `SDL_RenderSetLogicalSize`

L'analyse initiale suggérait `set_logical_size`, mais l'examen du code a révélé que `RenderRect::window_to_normalized()` (mod.rs L80-90) convertit les coordonnées souris fenêtre → normalisées en tenant compte du offset et du scaling du rect. Utiliser `set_logical_size` aurait causé une double application du scaling, cassant le mapping souris. La solution retenue conserve le système `RenderRect` existant et corrige son initialisation.

---

## Résolution de problèmes de déploiement (hors patches)

Pendant la validation en conditions réelles, plusieurs problèmes de déploiement (antérieurs aux patches) ont été identifiés et résolus :

| Problème | Cause | Solution |
|---|---|---|
| JWT refusé par le proxy-encoder | `jwt_secret` différent entre broker et proxy-encoder | Synchronisation de la clé |
| Agent vsock en boucle de retry | `backend = "stub"` dans la config proxy-encoder au lieu de `"vsock"` | Changement de backend |
| Client en mode renderer stub | Build sans `--features full` (default = `["stub"]`) | Rebuild avec `--features "sdl2-renderer openh264 x11-clipboard wayland-clipboard"` |

---

## Résultats mesurés

### En 1280×720

| Métrique | Avant | Après |
|---|---|---|
| Temps pour 300 frames | ~16 minutes (~0.3 fps) | ~20 secondes (~15 fps) |
| Frames dropped (décodeur) | 0 | 0 |
| Warnings "frames perdues" (proxy) | Fréquents | Aucun |
| Latence perçue souris/clavier | Visible | Non perceptible |
| Scaling au démarrage | Image décalée | Correct dès la première frame |

### En 1920×1080

| Métrique | Avant | Après |
|---|---|---|
| Temps pour 300 frames | ~16 minutes | ~20 secondes |
| Warnings "frames perdues" (proxy) | — | Réapparaissent (sporadiques) |
| Latence perçue souris | — | Légère, perceptible sur la souris |

Le 1080p reste limité par l'encodage openh264 software (conversion BGRA→RGB pixel par pixel + encodage CPU). Ce sera traité en optimisation future.

---

## Optimisations identifiées mais non appliquées

| # | Optimisation | Gain estimé | Complexité |
|---|---|---|---|
| O1 | `max_fps` adaptatif selon résolution (30 en 720p, 20 en 1080p) | Élimine les warnings 1080p | Faible |
| O2 | Conversion BGRA→RGB en `chunks_exact(4)` (encodeur + décodeur) | ~4-6 ms/frame (~15-18% du budget) | Faible |
| O3 | Pipeline VA-API réel (décodeur FFmpeg via `ffmpeg-sys-next`) | Décodage GPU ~0.3 ms vs ~3-5 ms CPU | Élevée |
| O4 | Texture SDL2 avec upload GPU direct | Élimination de la copie CPU ligne par ligne | Moyenne |
| O5 | Encodage hardware VAAPI côté proxy-encoder | Gain majeur en 1080p | Élevée |

---

## Intégration dans le plan Phase 2 (étape 7)

Ce travail s'intercale dans le plan de l'étape 7 (migration Proxmox → KVM/libvirt) qui comprend :

- **Phase 1** ✅ — Migration VM Proxmox → KVM/libvirt pur, tag `v0.7.1-proxmox-final`
- **Phase 1bis** ✅ — Fix session portail XDG unifiée, tag `v0.7.2-etape6j-portal-unifie`
- **Phase 1ter** ✅ — **Correctifs pipeline client/encodeur (cette session)**
  - Buffer decode→render 4→1
  - Scaling SDL2 (output_size, stream_w/h, recréation texture)
  - GOP et max_fps
  - Fix log hw= trompeur
  - Script `nidan-v2-check.py` (diagnostic + backup/rollback)
  - **Tag suggéré : `v0.7.3-client-perf`**
- **Phase 2** 🔜 — Extraction trait `VmProvider` + feature-gate Proxmox (script `phase2-apply.py` prêt)
- **Phase 3** — Implémentation `VmProvider` pour libvirt
- **Phase 4** — Reprise échelons C-G du pool dynamique sur le trait

---

## Livrables

| Fichier | Description |
|---|---|
| `nidan-v2-check.py` | Script de diagnostic et correction automatisé (dry-run/apply/rollback) |
| `nidan-v2-client-fixes.md` | Document technique détaillé (diagnostic + plan d'action) |
| Binaires recompilés | `nidan-client` (features full) + `nidan-proxy-encoder` |

---

## Commandes de référence

```bash
# Diagnostic (dry-run)
python3 nidan-v2-check.py --repo ~/Documents/NIDAN_SECURITY/nidan-v2

# Appliquer les corrections
python3 nidan-v2-check.py --repo ~/Documents/NIDAN_SECURITY/nidan-v2 --apply

# Rollback si nécessaire
python3 nidan-v2-check.py --repo ~/Documents/NIDAN_SECURITY/nidan-v2 --rollback

# Rebuild
cd ~/Documents/NIDAN_SECURITY/nidan-v2
cargo build --release -p nidan-client --no-default-features --features "sdl2-renderer openh264 x11-clipboard wayland-clipboard"
cargo build --release -p nidan-proxy-encoder

# Déploiement
sudo cp target/release/nidan-client /usr/bin/nidan-client
sudo cp target/release/nidan-proxy-encoder /opt/nidan/nidan-proxy-encoder

# Tag git
git add -A && git commit -m "fix(client+encoder): scaling SDL2, buffer latence, GOP, texture resize"
git tag v0.7.3-client-perf
```
