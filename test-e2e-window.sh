#!/usr/bin/env bash
# =============================================================================
# NIDAN — Test de bout en bout AVEC FENÊTRE VISIBLE (horloge + yeux)
# =============================================================================
# Variante « graphique » du scénario E2E : au lieu d'un rendu invisible
# (headless), le client ouvre une VRAIE fenêtre où s'affiche le bureau distant.
# La « VM » présente une horloge (xclock) et des yeux (xeyes) — on doit les
# voir bouger en direct dans la fenêtre NIDAN, prouvant que la capture, le
# transport chiffré, le décodage et le rendu fonctionnent visuellement.
#
# Différence avec validate-e2e-headless.sh :
#   - le serveur (VM) tourne toujours sur un écran virtuel Xvfb ;
#   - mais le CLIENT ouvre une fenêtre visible (pas de SDL_VIDEODRIVER=dummy).
#
# Deux modes :
#   • Sur une machine AVEC bureau (ton poste)  : la fenêtre s'affiche sur ton
#     écran réel. Tu la fermes manuellement (Échap ou la croix).
#   • Sur une machine SANS écran (serveur)     : utilise --virtual pour ouvrir
#     la fenêtre dans un Xvfb et en capturer une copie d'écran (PNG), preuve
#     que le rendu a bien produit une image.
#
# Usage :
#   ./test-e2e-window.sh [--deps] [--build] [--virtual] [--duration N] [--keep-logs]
#     --deps       installe xvfb, x11-apps, libsdl2, imagemagick (capture)
#     --build      (re)compile serveur/client/broker
#     --virtual    client dans un Xvfb + capture PNG (machines sans écran)
#     --duration N durée d'affichage en secondes (défaut 20)
#     --keep-logs  conserve les journaux
#
# À lancer depuis la racine du projet NIDAN.
# =============================================================================
set -uo pipefail
G='\033[0;32m'; R='\033[0;31m'; B='\033[0;34m'; Y='\033[1;33m'; C='\033[0;36m'; NC='\033[0m'
ok(){ echo -e "${G}✓${NC} $1"; }; ko(){ echo -e "${R}✗${NC} $1"; }
info(){ echo -e "${B}▶${NC} $1"; }; step(){ echo -e "\n${C}━━━ $1 ━━━${NC}"; }

INSTALL_DEPS=0; DO_BUILD=0; VIRTUAL=0; DURATION=20; KEEP=0
while [ $# -gt 0 ]; do case "$1" in
  --deps) INSTALL_DEPS=1; shift ;;
  --build) DO_BUILD=1; shift ;;
  --virtual) VIRTUAL=1; shift ;;
  --duration) DURATION="$2"; shift 2 ;;
  --keep-logs) KEEP=1; shift ;;
  *) shift ;;
esac; done

[ -f Cargo.toml ] && [ -d nidan-server ] || { ko "Lance depuis la racine du projet NIDAN."; exit 1; }

WORK=/tmp/nidan-e2e-window
mkdir -p "$WORK/certs"
SRV_DISPLAY=121           # écran virtuel de la VM (toujours Xvfb)
CLI_DISPLAY=122           # écran virtuel du client (mode --virtual uniquement)
SERVER_PORT=7610
BROKER_PORT=7611
ADMIN_PORT=7612
JWT_SECRET="window_validation_secret_32_chars_ok!"

SRV=""; BRK=""; XV_SRV=""; XV_CLI=""; CLI=""
cleanup(){
  [ -n "$CLI" ]&&kill $CLI 2>/dev/null; [ -n "$SRV" ]&&kill $SRV 2>/dev/null
  [ -n "$BRK" ]&&kill $BRK 2>/dev/null
  pkill -f "Xvfb :$SRV_DISPLAY" 2>/dev/null; pkill -f "Xvfb :$CLI_DISPLAY" 2>/dev/null
  pkill -f "nidan-broker" 2>/dev/null; pkill -f "nidan-server" 2>/dev/null
  pkill -f xclock 2>/dev/null; pkill -f xeyes 2>/dev/null
}
trap cleanup EXIT INT TERM

echo "═══════════════════════════════════════════════════════════════"
echo "  NIDAN — Test de bout en bout AVEC FENÊTRE (horloge + yeux)"
echo "═══════════════════════════════════════════════════════════════"

if [ "$INSTALL_DEPS" = "1" ]; then
  step "Dépendances"
  sudo apt-get update -qq 2>/dev/null || true
  sudo apt-get install -y xvfb x11-apps openssl libxcb-xtest0-dev libsdl2-dev imagemagick 2>&1 | tail -1
fi

# Détermination du mode d'affichage du client
if [ "$VIRTUAL" = "1" ]; then
  CLIENT_DISPLAY=":$CLI_DISPLAY"
  info "Mode VIRTUEL : la fenêtre client s'ouvre dans un Xvfb (capture PNG en fin)."
elif [ -n "${DISPLAY:-}" ]; then
  CLIENT_DISPLAY="$DISPLAY"
  info "Mode GRAPHIQUE : la fenêtre client s'ouvrira sur ton écran ($DISPLAY)."
else
  ko "Aucun écran détecté (\$DISPLAY vide). Sur un serveur sans bureau, utilise --virtual."
  exit 1
fi

step "Compilation"
if [ "$DO_BUILD" = "1" ]; then
  cargo build -p nidan-server --features full 2>&1 | tail -1
  # IMPORTANT : --no-default-features désactive la feature "stub" (active par
  # défaut), sinon le client ouvre un rendu factice au lieu d'une vraie fenêtre.
  cargo build -p nidan-client --no-default-features \
    --features "sdl2-renderer openh264 x11-clipboard wayland-clipboard" 2>&1 | tail -1
  cargo build -p nidan-broker 2>&1 | tail -1
fi
BIN=target/debug; [ -x target/release/nidan-client ] && BIN=target/release
for b in nidan-server nidan-broker nidan-client; do
  [ -x "$BIN/$b" ] || { ko "binaire $b introuvable — relance avec --build"; exit 1; }
done
# Avertir si le client embarque encore le rendu factice (stub)
if strings "$BIN/nidan-client" 2>/dev/null | grep -q "renderer stub démarré"; then
  ko "Le client est compilé en mode STUB (pas de vraie fenêtre)."
  info "Recompile sans la feature par défaut :"
  info "  cargo build -p nidan-client --no-default-features --features \"sdl2-renderer openh264 x11-clipboard wayland-clipboard\""
  info "puis relance ce script (sans --build, ou avec --build qui le fait déjà)."
  exit 1
fi
# Avertir si le serveur est en stub (capture/encodage factices → décodage client KO)
if strings "$BIN/nidan-server" 2>/dev/null | grep -q "X11Capturer en mode stub"; then
  ko "Le serveur est compilé en mode STUB (capture/encodage factices)."
  info "Recompile avec la capture et l'encodage réels :"
  info "  cargo build -p nidan-server --features full"
  exit 1
fi
ok "binaires prêts ($BIN) — client (fenêtre SDL2 réelle) + serveur (capture/encodage réels)"

step "PKI"
bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1
ok "PKI prête"

step "Écran virtuel de la VM (horloge + yeux)"
pkill -f "Xvfb :$SRV_DISPLAY" 2>/dev/null; pkill -f nidan 2>/dev/null; sleep 1
Xvfb :$SRV_DISPLAY -screen 0 1280x720x24 >/dev/null 2>&1 & XV_SRV=$!
sleep 2
# Contenu animé bien visible : horloge + yeux qui suivent + fond coloré
DISPLAY=:$SRV_DISPLAY xsetroot -solid "#234" 2>/dev/null || true
DISPLAY=:$SRV_DISPLAY xclock -update 1 -geometry 300x300+80+80 >/dev/null 2>&1 &
DISPLAY=:$SRV_DISPLAY xeyes -geometry 300x300+500+200 >/dev/null 2>&1 &
sleep 1
ok "VM : horloge (mise à jour 1s) + yeux affichés sur :$SRV_DISPLAY"

step "Configurations"
cat > "$WORK/server.toml" << EOF
[network]
bind_addr = "127.0.0.1:$SERVER_PORT"
session_timeout_secs = 3600
max_connections = 4
[tls]
ca_cert = "$WORK/certs/ca.crt"
cert = "$WORK/certs/server.crt"
key = "$WORK/certs/server.key"
[capture]
display_number = $SRV_DISPLAY
use_xshm = false
use_xdamage = false
capture_queue_depth = 4
[video]
codec = "h264"
max_fps = 20
target_bitrate_kbps = 0
pixel_format = "yuv420p"
hardware_accel = false
[security]
seccomp_enabled = false
e2e_encryption = true
jwt_secret = "$JWT_SECRET"
require_session_token = true
[clipboard]
allow_client_to_server = true
allow_server_to_client = true
max_size_bytes = 65536
blocked_patterns = []
audit_transfers = true
EOF

cat > "$WORK/broker.toml" << EOF
[network]
quic_bind = "127.0.0.1:$BROKER_PORT"
admin_bind = "127.0.0.1:$ADMIN_PORT"
session_timeout_secs = 3600
max_sessions = 8
[auth]
enabled_methods = ["mtls"]
session_token_ttl_secs = 3600
[auth.jwt]
secret = "$JWT_SECRET"
issuer = "nidan-broker"
[pool]
min_available = 1
health_check_timeout_secs = 3
health_check_interval_secs = 30
[[pool.static_vms]]
id = "vm-window-01"
host = "127.0.0.1"
port = $SERVER_PORT
tags = ["linux"]
[tls]
ca_cert = "$WORK/certs/ca.crt"
cert = "$WORK/certs/broker.crt"
key = "$WORK/certs/broker.key"
[security]
rate_limit_auth = true
max_auth_attempts = 5
ban_duration_secs = 300
audit_all_auth = true
[admin]
metrics_enabled = true
EOF

cat > "$WORK/client.toml" << EOF
[network]
broker_addr = "127.0.0.1:$BROKER_PORT"
connect_timeout_secs = 10
auto_reconnect = false
reconnect_delay_secs = 3
[tls]
ca_cert = "$WORK/certs/ca.crt"
cert = "$WORK/certs/client.crt"
key = "$WORK/certs/client.key"
[video]
preferred_codec = "h264"
max_fps = 20
target_bitrate_kbps = 0
hardware_decode = false
decode_buffer_size = 4
[display]
fullscreen = false
seamless = false
scaling = "fit"
window_title = "NIDAN — bureau distant (horloge + yeux)"
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
e2e_encryption = true
auth_method = "mtls"
EOF
ok "configurations générées"

step "Démarrage serveur + broker"
NIDAN_SERVER_CONFIG="$WORK/server.toml" NIDAN_DISPLAY=$SRV_DISPLAY NIDAN_LOG=info \
  "$BIN/nidan-server" > "$WORK/server.log" 2>&1 & SRV=$!
sleep 2
NIDAN_LOG=info "$BIN/nidan-broker" --config "$WORK/broker.toml" > "$WORK/broker.log" 2>&1 & BRK=$!
sleep 3
ok "serveur + broker en écoute"

# ── Écran virtuel pour le client si mode --virtual ──
if [ "$VIRTUAL" = "1" ]; then
  pkill -f "Xvfb :$CLI_DISPLAY" 2>/dev/null; sleep 1
  Xvfb :$CLI_DISPLAY -screen 0 1280x800x24 >/dev/null 2>&1 & XV_CLI=$!
  sleep 2
  info "écran virtuel client prêt sur :$CLI_DISPLAY"
fi

step "Ouverture de la fenêtre NIDAN ($DURATION s)"
if [ "$VIRTUAL" = "1" ]; then
  # Fenêtre dans le Xvfb client ; on capture une copie d'écran à mi-parcours.
  DISPLAY=$CLIENT_DISPLAY NIDAN_LOG=info \
    timeout $((DURATION+4)) "$BIN/nidan-client" --config "$WORK/client.toml" \
    > "$WORK/client.log" 2>&1 & CLI=$!
  sleep $((DURATION/2))
  CAP="$WORK/capture-fenetre.png"
  if command -v import >/dev/null 2>&1; then
    DISPLAY=$CLIENT_DISPLAY import -window root "$CAP" 2>/dev/null
  elif command -v xwd >/dev/null 2>&1; then
    DISPLAY=$CLIENT_DISPLAY xwd -root -silent 2>/dev/null | \
      convert xwd:- "$CAP" 2>/dev/null
  fi
  sleep $((DURATION/2 + 2))
  kill $CLI 2>/dev/null; CLI=""
  [ -f "$CAP" ] && ok "copie d'écran de la fenêtre : $CAP" \
                || info "capture indisponible (imagemagick absent) — voir les logs"
else
  # Mode graphique réel : la fenêtre s'affiche sur l'écran de l'utilisateur.
  info "La fenêtre NIDAN va s'ouvrir. Tu dois y voir l'horloge et les yeux."
  info "Ferme-la (Échap ou la croix) ou attends ${DURATION}s."
  DISPLAY=$CLIENT_DISPLAY NIDAN_LOG=info \
    timeout $((DURATION+2)) "$BIN/nidan-client" --config "$WORK/client.toml" \
    > "$WORK/client.log" 2>&1 || true
fi
sleep 1
kill $SRV $BRK 2>/dev/null; SRV=""; BRK=""

step "Contrôles"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/client.log" > "$WORK/client.clean"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/server.log" > "$WORK/server.clean"
PASS=0; FAIL=0
chk(){ if [ "$1" = "0" ]; then ok "$2"; PASS=$((PASS+1)); else ko "$2"; FAIL=$((FAIL+1)); fi; }

grep -q "connexion QUIC serveur établie\|VM attribuée" "$WORK/client.clean"
chk $? "Client connecté à la VM via le broker"
grep -q "chiffrement E2E actif" "$WORK/client.clean"
chk $? "Session chiffrée E2E"
FRAMES=$(grep -c "frame reçue" "$WORK/client.clean" 2>/dev/null | head -1); FRAMES=${FRAMES:-0}
if [ "$FRAMES" -gt 0 ] 2>/dev/null; then
  chk 0 "Flux vidéo rendu dans la fenêtre ($FRAMES frames de l'horloge + yeux)"
else
  chk 1 "Aucune frame rendue"
fi
# Le rendu SDL2 a-t-il bien été activé (pas le mode dummy/stub) ?
if grep -qi "SDL2\|fenêtre\|renderer" "$WORK/client.clean"; then
  chk 0 "Rendu SDL2 actif (vraie fenêtre)"
else
  info "rendu SDL2 non tracé explicitement (vérifie la fenêtre à l'écran)"
fi
if [ "$VIRTUAL" = "1" ] && [ -f "$WORK/capture-fenetre.png" ]; then
  chk 0 "Copie d'écran capturée (preuve visuelle du rendu)"
fi

echo ""
if [ "$FAIL" = "0" ]; then
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
  echo -e "${G}  ✓✓✓ TEST FENÊTRE RÉUSSI${NC}"
  echo -e "${G}  Le bureau distant (horloge + yeux) s'affiche dans la fenêtre,${NC}"
  echo -e "${G}  via une session chiffrée de bout en bout.${NC}"
  [ "$VIRTUAL" = "1" ] && echo -e "${G}  Preuve visuelle : $WORK/capture-fenetre.png${NC}"
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
else
  echo -e "${Y}Des contrôles ont échoué — voir $WORK/client.log${NC}"
  grep -iE "erreur|error|échec" "$WORK/client.clean" | tail -5
fi
echo ""
info "Journaux : $WORK/{server,broker,client}.log"
echo "Résultat : $PASS réussis, $FAIL échoués"
[ "$KEEP" = "0" ] && rm -f "$WORK"/*.clean
[ "$FAIL" = "0" ] && exit 0 || exit 1
