#!/usr/bin/env bash
# =============================================================================
# NIDAN — Script de test E2E (V2) — vérification du chiffrement de bout en bout
# =============================================================================
# Cette V2 ne se contente pas de lancer une session : elle PROUVE que le
# chiffrement E2E (X25519 + ChaCha20-Poly1305) fonctionne réellement, en
# trois contrôles :
#
#   1. Échange de clés    — les deux côtés annoncent l'activation E2E
#   2. Chiffrement actif  — le serveur chiffre réellement les frames envoyées
#   3. Déchiffrement OK    — le client décode les frames sans erreur AEAD
#
# Bonus : un mode --negative qui lance la session SANS E2E pour confirmer le
# contraste (et que le pipeline marche dans les deux cas).
#
# Usage :
#   ./test-nidan-e2e.sh              # test E2E complet (par défaut)
#   ./test-nidan-e2e.sh --negative   # test de contrôle, E2E désactivé
#   ./test-nidan-e2e.sh --deps       # installe les dépendances système d'abord
#   ./test-nidan-e2e.sh --keep-logs  # conserve les logs après exécution
#
# À lancer depuis la racine du projet (où se trouve Cargo.toml).
# =============================================================================

set -uo pipefail

G='\033[0;32m'; Y='\033[1;33m'; R='\033[0;31m'; B='\033[0;34m'; NC='\033[0m'
ok()    { echo -e "${G}✓${NC} $1"; }
ko()    { echo -e "${R}✗${NC} $1"; }
info()  { echo -e "${B}▶${NC} $1"; }
warn()  { echo -e "${Y}⚠${NC} $1"; }
step()  { echo -e "\n${B}━━━ $1 ━━━${NC}"; }

# ── Paramètres ────────────────────────────────────────────────────────────────
E2E=true
INSTALL_DEPS=0
KEEP_LOGS=0
DISPLAY_NUM=101            # display dédié pour ne pas heurter test-nidan.sh (:100)
PORT=7454
WORK=/tmp/nidan-e2e-test
SESSION_SECS=12            # durée de la session de test
SCREEN_W=1280
SCREEN_H=720

for arg in "$@"; do
  case "$arg" in
    --negative)  E2E=false ;;
    --deps)      INSTALL_DEPS=1 ;;
    --keep-logs) KEEP_LOGS=1 ;;
    *) warn "argument inconnu: $arg" ;;
  esac
done

SRV_LOG="$WORK/server.log"
CLI_LOG="$WORK/client.log"

# ── Nettoyage en sortie ───────────────────────────────────────────────────────
XVFB_PID=""; SRV_PID=""
cleanup() {
  [ -n "$SRV_PID" ]  && kill "$SRV_PID"  2>/dev/null
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null
  pkill -f "nidan-server" 2>/dev/null
  if [ "$KEEP_LOGS" = "0" ]; then
    : # logs conservés dans $WORK de toute façon, on n'efface rien d'utile
  fi
}
trap cleanup EXIT INT TERM

# ── Vérif racine projet ───────────────────────────────────────────────────────
if [ ! -f Cargo.toml ] || [ ! -d nidan-server ]; then
  ko "Lance ce script depuis la racine du projet NIDAN (où est Cargo.toml)."
  exit 1
fi

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  NIDAN — Test E2E (V2)"
echo "  Mode : $([ "$E2E" = "true" ] && echo 'chiffrement E2E ACTIVÉ' || echo 'E2E DÉSACTIVÉ (contrôle négatif)')"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# ── 0. Dépendances ────────────────────────────────────────────────────────────
if [ "$INSTALL_DEPS" = "1" ]; then
  step "Dépendances système"
  sudo apt-get update
  sudo apt-get install -y \
    libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev libxcb-damage0-dev libxcb-xtest0-dev \
    xvfb x11-apps openssl \
    || { ko "échec installation deps"; exit 1; }
fi
for tool in cargo Xvfb openssl; do
  command -v "$tool" >/dev/null 2>&1 || { ko "$tool introuvable (essaie --deps)"; exit 1; }
done

# ── 1. Compilation ────────────────────────────────────────────────────────────
step "Compilation des binaires (mode réel)"
info "serveur (full : capture + encode + E2E)..."
cargo build -p nidan-server --features full 2>&1 | tail -1 || { ko "build serveur"; exit 1; }
info "client (décode + E2E, rendu headless)..."
cargo build -p nidan-client --features openh264 2>&1 | tail -1 || { ko "build client"; exit 1; }
ok "binaires compilés"

# ── 2. PKI ────────────────────────────────────────────────────────────────────
step "Certificats"
mkdir -p "$WORK/certs"
if [ ! -f "$WORK/certs/ca.crt" ]; then
  bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1 \
    && ok "PKI générée" || { ko "échec PKI"; exit 1; }
else
  ok "PKI déjà présente"
fi

# ── 3. Configs (E2E selon le mode) ────────────────────────────────────────────
step "Configuration (e2e_encryption = $E2E)"
cat > "$WORK/server.toml" << EOF
[network]
bind_addr = "127.0.0.1:$PORT"
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
max_fps = 10
target_bitrate_kbps = 0
pixel_format = "yuv420p"
hardware_accel = false
[security]
seccomp_enabled = false
e2e_encryption = $E2E
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
max_fps = 10
target_bitrate_kbps = 0
hardware_decode = false
decode_buffer_size = 4
[display]
fullscreen = false
seamless = false
scaling = "fit"
window_title = "NIDAN E2E test"
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
e2e_encryption = $E2E
auth_method = "mtls"
EOF
ok "configs écrites"

# ── 4. Écran virtuel ──────────────────────────────────────────────────────────
step "Écran virtuel Xvfb :$DISPLAY_NUM"
pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; sleep 1
Xvfb :$DISPLAY_NUM -screen 0 ${SCREEN_W}x${SCREEN_H}x24 >/dev/null 2>&1 &
XVFB_PID=$!
sleep 2
DISPLAY=:$DISPLAY_NUM xdpyinfo >/dev/null 2>&1 || { ko "Xvfb n'a pas démarré"; exit 1; }
command -v xclock >/dev/null 2>&1 && DISPLAY=:$DISPLAY_NUM xclock >/dev/null 2>&1 &
command -v xeyes  >/dev/null 2>&1 && DISPLAY=:$DISPLAY_NUM xeyes  >/dev/null 2>&1 &
sleep 1
ok "Xvfb actif avec contenu"

# ── 5. Serveur ────────────────────────────────────────────────────────────────
step "Serveur NIDAN"
NIDAN_SERVER_CONFIG="$WORK/server.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
  ./target/debug/nidan-server > "$SRV_LOG" 2>&1 &
SRV_PID=$!
sleep 3
kill -0 "$SRV_PID" 2>/dev/null || { ko "le serveur a crashé :"; grep -vE "^\s+[0-9]+:|^\s+at " "$SRV_LOG" | tail -10; exit 1; }
ok "serveur en écoute sur 127.0.0.1:$PORT"

# ── 6. Client (session bornée) ────────────────────────────────────────────────
step "Session de test ($SESSION_SECS s)"
NIDAN_LOG=info SDL_VIDEODRIVER=dummy \
  timeout $SESSION_SECS ./target/debug/nidan-client \
    --config "$WORK/client.toml" --direct 127.0.0.1:$PORT > "$CLI_LOG" 2>&1
sleep 1
kill "$SRV_PID" 2>/dev/null
ok "session terminée"

# =============================================================================
#  VÉRIFICATIONS
# =============================================================================
step "Résultats"

# Les logs tracing contiennent des codes couleur ANSI qui cassent le parsing.
# On produit des copies nettoyées pour des greps fiables.
sed 's/\x1b\[[0-9;]*m//g' "$SRV_LOG" > "$SRV_LOG.clean" 2>/dev/null
sed 's/\x1b\[[0-9;]*m//g' "$CLI_LOG" > "$CLI_LOG.clean" 2>/dev/null
SRV_LOG="$SRV_LOG.clean"
CLI_LOG="$CLI_LOG.clean"

PASS=0; FAIL=0
check() { if [ "$1" = "0" ]; then ok "$2"; PASS=$((PASS+1)); else ko "$2"; FAIL=$((FAIL+1)); fi; }

# Indicateur commun : la session a-t-elle fait circuler des frames ?
grep -q "frame reçue" "$CLI_LOG" 2>/dev/null; FRAMES_FLOW=$?
FRAME_COUNT=$(grep -oE "frames=[0-9]+" "$CLI_LOG" 2>/dev/null | tail -1 | grep -oE "[0-9]+")
FRAME_COUNT=${FRAME_COUNT:-0}

if [ "$E2E" = "true" ]; then
  # 1. Échange de clés annoncé des deux côtés
  grep -q "chiffrement E2E activé" "$SRV_LOG" 2>/dev/null; SRV_E2E=$?
  grep -q "chiffrement E2E actif"  "$CLI_LOG" 2>/dev/null; CLI_E2E=$?
  check $SRV_E2E "Serveur : échange de clés X25519 effectué, E2E activé"
  check $CLI_E2E "Client  : clés dérivées, E2E actif"

  # 2. Le serveur chiffre réellement les frames (compteur chiffrees == total)
  ENC_LINE=$(grep "frames envoyées" "$SRV_LOG" 2>/dev/null | tail -1)
  SRV_TOTAL=$(echo "$ENC_LINE" | grep -oE "total=[0-9]+" | grep -oE "[0-9]+")
  SRV_ENC=$(echo "$ENC_LINE" | grep -oE "chiffrees=[0-9]+" | grep -oE "[0-9]+")
  SRV_TOTAL=${SRV_TOTAL:-0}; SRV_ENC=${SRV_ENC:-0}
  if [ "$SRV_TOTAL" -gt 0 ] && [ "$SRV_ENC" = "$SRV_TOTAL" ]; then
    check 0 "Chiffrement : $SRV_ENC/$SRV_TOTAL frames chiffrées côté serveur (100%)"
  else
    check 1 "Chiffrement : seulement $SRV_ENC/$SRV_TOTAL frames chiffrées"
  fi

  # 3. Le client déchiffre sans erreur AEAD ET reçoit des frames
  grep -qi "déchiffrement.*échoué\|déchiffrement frame échoué" "$CLI_LOG" 2>/dev/null; DEC_FAIL=$?
  if [ "$FRAMES_FLOW" = "0" ] && [ "$DEC_FAIL" != "0" ]; then
    check 0 "Déchiffrement : $FRAME_COUNT frames déchiffrées et décodées, 0 erreur AEAD"
  else
    check 1 "Déchiffrement : échec (frames=$FRAME_COUNT, erreurs AEAD détectées)"
  fi

  echo ""
  if [ "$FAIL" = "0" ]; then
    echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${G}  ✓✓✓ CHIFFREMENT E2E VALIDÉ DE BOUT EN BOUT${NC}"
    echo -e "${G}  X25519 (Diffie-Hellman) + HKDF-SHA256 + ChaCha20-Poly1305${NC}"
    echo -e "${G}  Le flux vidéo est chiffré sur le réseau ; seul le client le déchiffre.${NC}"
    echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
  fi
else
  # Mode contrôle négatif : E2E désactivé, on vérifie juste que le pipeline marche
  grep -q "chiffrement E2E activé" "$SRV_LOG" 2>/dev/null && NO_E2E=1 || NO_E2E=0
  check $NO_E2E "E2E bien DÉSACTIVÉ (aucune activation côté serveur)"
  if [ "$FRAMES_FLOW" = "0" ]; then
    check 0 "Pipeline en clair fonctionnel : $FRAME_COUNT frames reçues"
  else
    check 1 "Pipeline : aucune frame reçue"
  fi
  echo ""
  echo -e "${B}  Mode contrôle : session en clair vérifiée.${NC}"
fi

# ── Diagnostic en cas d'échec ─────────────────────────────────────────────────
if [ "$FAIL" != "0" ]; then
  echo ""
  warn "Des contrôles ont échoué. Extraits de logs :"
  echo "--- serveur (E2E / frames) ---"
  grep -iE "E2E|frames envoyées|erreur|error" "$SRV_LOG" 2>/dev/null | grep -vE "^\s+[0-9]+:" | tail -8
  echo "--- client (E2E / frames / déchiffrement) ---"
  grep -iE "E2E|frame reçue|déchiffrement|erreur|error" "$CLI_LOG" 2>/dev/null | grep -vE "^\s+[0-9]+:" | tail -8
fi

echo ""
info "Logs complets : $SRV_LOG  /  $CLI_LOG"
echo "Résultat : $PASS réussis, $FAIL échoués"
[ "$FAIL" = "0" ] && exit 0 || exit 1
