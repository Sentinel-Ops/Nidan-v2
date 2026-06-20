#!/usr/bin/env bash
# =============================================================================
# NIDAN — Script de test end-to-end sur PC (avec écran virtuel Xvfb)
# =============================================================================
# Lance une session NIDAN complète sur une seule machine :
#   - Xvfb : écran virtuel que le serveur va capturer
#   - nidan-server : capture X11 + encode H.264 + injection inputs
#   - nidan-client : décode H.264 + affichage SDL2 (fenêtre sur ton bureau)
#
# Usage :
#   ./test-nidan.sh              # session interactive (fenêtre SDL2)
#   ./test-nidan.sh --headless   # test auto sans fenêtre (vérifie le flux)
#   ./test-nidan.sh --deps       # installe d'abord les dépendances système
#   ./test-nidan.sh --build-only # compile seulement, ne lance rien
#
# À lancer depuis la racine du projet (où se trouve Cargo.toml).
# =============================================================================

set -uo pipefail

# ── Couleurs ──────────────────────────────────────────────────────────────────
G='\033[0;32m'; Y='\033[1;33m'; R='\033[0;31m'; B='\033[0;34m'; NC='\033[0m'
info()  { echo -e "${G}▶${NC} $1"; }
warn()  { echo -e "${Y}⚠${NC} $1"; }
err()   { echo -e "${R}✗${NC} $1"; }
step()  { echo -e "\n${B}━━━ $1 ━━━${NC}"; }

# ── Paramètres ────────────────────────────────────────────────────────────────
HEADLESS=0
INSTALL_DEPS=0
BUILD_ONLY=0
DISPLAY_NUM=100
WORK=/tmp/nidan-test
SCREEN_W=1280
SCREEN_H=720

for arg in "$@"; do
  case "$arg" in
    --headless)   HEADLESS=1 ;;
    --deps)       INSTALL_DEPS=1 ;;
    --build-only) BUILD_ONLY=1 ;;
    *) warn "argument inconnu: $arg" ;;
  esac
done

# ── PIDs à nettoyer ───────────────────────────────────────────────────────────
XVFB_PID=""; SRV_PID=""; APPS_PIDS=()
cleanup() {
  step "Nettoyage"
  [ -n "$SRV_PID" ]  && kill "$SRV_PID"  2>/dev/null && info "serveur arrêté"
  for p in "${APPS_PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null && info "Xvfb arrêté"
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null
  info "terminé."
}
trap cleanup EXIT INT TERM

# ── Vérif : on est à la racine du projet ? ────────────────────────────────────
if [ ! -f Cargo.toml ] || [ ! -d nidan-server ]; then
  err "Lance ce script depuis la racine du projet NIDAN (où est Cargo.toml)."
  exit 1
fi

# ── 0. Dépendances système ────────────────────────────────────────────────────
if [ "$INSTALL_DEPS" = "1" ]; then
  step "Installation des dépendances système"
  sudo apt-get update
  sudo apt-get install -y \
    libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev libxcb-damage0-dev libxcb-xtest0-dev \
    libsdl2-dev xvfb x11-apps openssl \
    || { err "échec installation deps"; exit 1; }
  info "dépendances installées"
fi

# Vérifier que les outils nécessaires sont présents
for tool in cargo Xvfb openssl; do
  command -v "$tool" >/dev/null 2>&1 || {
    err "$tool introuvable. Lance : ./test-nidan.sh --deps (ou installe Rust pour cargo)"
    exit 1
  }
done

# ── 1. Compilation ────────────────────────────────────────────────────────────
step "Compilation (mode réel)"

info "serveur (capture X11 + encode H.264 + XTEST)..."
if ! cargo build -p nidan-server --features full 2>&1 | tail -1; then
  err "échec compilation serveur"; exit 1
fi

if [ "$HEADLESS" = "1" ]; then
  info "client (décode H.264, rendu stub headless)..."
  cargo build -p nidan-client --features openh264 2>&1 | tail -1
else
  info "client (décode H.264 + rendu SDL2)..."
  cargo build -p nidan-client --no-default-features --features sdl2-renderer,openh264 2>&1 | tail -1
fi
info "compilation OK"

[ "$BUILD_ONLY" = "1" ] && { info "build-only : terminé."; exit 0; }

# ── 2. PKI ────────────────────────────────────────────────────────────────────
step "Génération des certificats"
mkdir -p "$WORK/certs"
if [ ! -f "$WORK/certs/ca.crt" ]; then
  bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1 \
    && info "certificats générés dans $WORK/certs" \
    || { err "échec génération PKI"; exit 1; }
else
  info "certificats déjà présents"
fi

# ── 3. Fichiers de config ─────────────────────────────────────────────────────
step "Configuration"
cat > "$WORK/server.toml" << EOF
[network]
bind_addr = "127.0.0.1:7444"
session_timeout_secs = 3600
max_connections = 1
[tls]
ca_cert = "$WORK/certs/ca.crt"
cert    = "$WORK/certs/server.crt"
key     = "$WORK/certs/server.key"
[capture]
display_number = $DISPLAY_NUM
use_xshm = false
use_xdamage = false
capture_queue_depth = 4
[video]
codec = "h264"
max_fps = 15
target_bitrate_kbps = 0
pixel_format = "yuv420p"
hardware_accel = false
[security]
seccomp_enabled = false
e2e_encryption = false
EOF

cat > "$WORK/client.toml" << EOF
[network]
broker_addr = "127.0.0.1:7443"
connect_timeout_secs = 10
auto_reconnect = false
reconnect_delay_secs = 3
[tls]
ca_cert = "$WORK/certs/ca.crt"
cert    = "$WORK/certs/client.crt"
key     = "$WORK/certs/client.key"
[video]
preferred_codec = "h264"
max_fps = 15
target_bitrate_kbps = 0
hardware_decode = false
decode_buffer_size = 4
[display]
fullscreen = false
seamless = false
scaling = "fit"
window_title = "NIDAN - bureau distant"
[input]
capture_system_shortcuts = false
mouse_sensitivity = 1.0
touch_enabled = false
escape_combo = "Ctrl+Alt+F"
[clipboard]
allow_client_to_server = true
allow_server_to_client = true
max_size_bytes = 1048576
audit_transfers = true
[security]
verify_server_cert = true
e2e_encryption = false
auth_method = "mtls"
EOF
info "configs écrites dans $WORK/"

# ── 4. Écran virtuel + applications ───────────────────────────────────────────
step "Écran virtuel Xvfb :$DISPLAY_NUM ($SCREEN_W x $SCREEN_H)"
pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; sleep 1
Xvfb :$DISPLAY_NUM -screen 0 ${SCREEN_W}x${SCREEN_H}x24 >/dev/null 2>&1 &
XVFB_PID=$!
sleep 2
if ! DISPLAY=:$DISPLAY_NUM xdpyinfo >/dev/null 2>&1; then
  err "Xvfb n'a pas démarré"; exit 1
fi
info "Xvfb actif (PID $XVFB_PID)"

# Lancer des applications visibles dans l'écran virtuel
if command -v xclock >/dev/null 2>&1; then
  DISPLAY=:$DISPLAY_NUM xclock -geometry 300x300+100+100 >/dev/null 2>&1 & APPS_PIDS+=($!)
  DISPLAY=:$DISPLAY_NUM xeyes  -geometry 200x200+700+200 >/dev/null 2>&1 & APPS_PIDS+=($!)
  command -v xterm >/dev/null 2>&1 && { DISPLAY=:$DISPLAY_NUM xterm -geometry 50x15+100+450 >/dev/null 2>&1 & APPS_PIDS+=($!); }
  info "applications lancées (xclock, xeyes, xterm) — bouge la souris pour voir les yeux suivre"
else
  warn "x11-apps absent (xclock/xeyes). Installe avec --deps pour voir un contenu animé."
fi
sleep 1

# ── 5. Serveur NIDAN ──────────────────────────────────────────────────────────
step "Démarrage du serveur NIDAN"
NIDAN_SERVER_CONFIG="$WORK/server.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
  ./target/debug/nidan-server > "$WORK/server.log" 2>&1 &
SRV_PID=$!
sleep 3
if ! kill -0 "$SRV_PID" 2>/dev/null; then
  err "le serveur a crashé. Log :"
  grep -vE "^\s+[0-9]+:|^\s+at " "$WORK/server.log" | tail -15
  exit 1
fi
info "serveur en écoute sur 127.0.0.1:7444 (PID $SRV_PID)"

# ── 6. Client NIDAN ───────────────────────────────────────────────────────────
step "Démarrage du client NIDAN"

if [ "$HEADLESS" = "1" ]; then
  info "mode headless : le client tourne 10s et vérifie la réception de frames"
  NIDAN_LOG=info SDL_VIDEODRIVER=dummy \
    timeout 10 ./target/debug/nidan-client \
      --config "$WORK/client.toml" --direct 127.0.0.1:7444 > "$WORK/client.log" 2>&1
  echo ""
  if grep -q "frame reçue" "$WORK/client.log" 2>/dev/null; then
    FRAMES=$(grep -oE "frames=[0-9]+" "$WORK/client.log" | tail -1)
    info "✓✓✓ SUCCÈS : le client a reçu des frames du serveur ($FRAMES)"
    info "Le pipeline complet fonctionne : capture → encode → QUIC → decode"
  else
    err "le client n'a pas reçu de frames. Logs :"
    echo "--- client ---"; grep -vE "^\s+[0-9]+:|^\s+at " "$WORK/client.log" | tail -12
    echo "--- serveur ---"; grep -vE "^\s+[0-9]+:|^\s+at " "$WORK/server.log" | tail -8
    exit 1
  fi
else
  info "une fenêtre SDL2 va s'ouvrir sur ton bureau"
  info "→ tu verras l'horloge et les yeux du bureau distant"
  info "→ bouge la souris dans la fenêtre : les yeux distants suivront (injection XTEST)"
  info "→ ferme la fenêtre ou Ctrl+C ici pour arrêter"
  echo ""
  NIDAN_LOG=info \
    ./target/debug/nidan-client \
      --config "$WORK/client.toml" --direct 127.0.0.1:7444
fi

step "Session terminée"
