#!/usr/bin/env bash
# =============================================================================
# NIDAN — Test de la Phase 15 : canal réseau du presse-papier + filtrage
# =============================================================================
# Prouve que le presse-papier transite chiffré sur le canal de contrôle et que
# le filtre de politique tranche réellement avant injection dans la VM.
#
#   A. Presse-papier anodin (texte) → ACCEPTÉ par le filtre
#   B. Secret (clé privée PEM)       → REFUSÉ par le filtre
#   C. Le transfert est bien chiffré E2E (le client log "E2E actif" avant envoi)
#
# Le client utilise le hook NIDAN_TEST_CLIPBOARD pour envoyer un presse-papier
# dès la connexion (pas besoin de capture X réelle).
#
# Usage : ./test-clipboard.sh [--deps] [--keep-logs]
# À lancer depuis la racine du projet NIDAN.
# =============================================================================
set -uo pipefail
G='\033[0;32m'; R='\033[0;31m'; B='\033[0;34m'; Y='\033[1;33m'; NC='\033[0m'
ok(){ echo -e "${G}✓${NC} $1"; }; ko(){ echo -e "${R}✗${NC} $1"; }
info(){ echo -e "${B}▶${NC} $1"; }; step(){ echo -e "\n${B}━━━ $1 ━━━${NC}"; }

INSTALL_DEPS=0; KEEP=0
for a in "$@"; do case "$a" in
  --deps) INSTALL_DEPS=1;; --keep-logs) KEEP=1;; esac; done

[ -f Cargo.toml ] && [ -d nidan-server ] || { ko "Lance depuis la racine du projet NIDAN."; exit 1; }
command -v cargo >/dev/null 2>&1 || { ko "cargo introuvable."; exit 1; }

WORK=/tmp/nidan-clipboard-test
mkdir -p "$WORK/certs"
DISPLAY_NUM=108; SERVER_PORT=7480

SRV=""; XV=""
cleanup(){
  [ -n "$SRV" ]&&kill $SRV 2>/dev/null
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f "nidan-server" 2>/dev/null
  pkill -f xclock 2>/dev/null
}
trap cleanup EXIT INT TERM

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  NIDAN — Test Phase 15 : canal réseau du presse-papier"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [ "$INSTALL_DEPS" = "1" ]; then
  step "Dépendances"
  sudo apt-get install -y xvfb x11-apps libxcb-xtest0-dev libsdl2-dev 2>&1 | tail -1
fi

step "Compilation + PKI"
cargo build -p nidan-server --features full 2>&1 | tail -1
cargo build -p nidan-client --features openh264 2>&1 | tail -1
[ -f "$WORK/certs/ca.crt" ] || bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1
ok "binaires + PKI prêts"

step "Configurations"
# Serveur : politique de filtrage — bloque les clés privées, c2s autorisé
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
e2e_encryption = true
[clipboard]
allow_client_to_server = true
allow_server_to_client = false
max_size_bytes = 65536
blocked_patterns = ["-----BEGIN [A-Z ]*PRIVATE KEY-----", "\\\\b\\\\d{4}[ -]?\\\\d{4}[ -]?\\\\d{4}[ -]?\\\\d{4}\\\\b"]
audit_transfers = true
EOF

cat > "$WORK/client.toml" << EOF
[network]
broker_addr = "127.0.0.1:7999"
connect_timeout_secs = 10
auto_reconnect = false
reconnect_delay_secs = 3
[tls]
ca_cert = "$WORK/certs/ca.crt"
cert = "$WORK/certs/client.crt"
key = "$WORK/certs/client.key"
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
window_title = "clipboard test"
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
ok "configs écrites (filtre serveur : bloque clés privées + numéros CB)"

step "Démarrage serveur + écran virtuel"
pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f nidan 2>/dev/null; sleep 1
Xvfb :$DISPLAY_NUM -screen 0 1280x720x24 >/dev/null 2>&1 & XV=$!
sleep 2
command -v xclock >/dev/null && DISPLAY=:$DISPLAY_NUM xclock >/dev/null 2>&1 &
sleep 1
NIDAN_SERVER_CONFIG="$WORK/server.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
  ./target/debug/nidan-server > "$WORK/server.log" 2>&1 & SRV=$!
sleep 2
ok "serveur démarré (port $SERVER_PORT)"

# ── Cas A : presse-papier anodin → accepté ───────────────────────────────────
step "A. Presse-papier anodin (doit être ACCEPTÉ)"
NIDAN_LOG=info SDL_VIDEODRIVER=dummy \
  NIDAN_TEST_CLIPBOARD="bonjour ceci est un texte normal et sans secret" \
  timeout 7 ./target/debug/nidan-client --config "$WORK/client.toml" \
    --direct 127.0.0.1:$SERVER_PORT > "$WORK/client_ok.log" 2>&1
sleep 1
ok "session A terminée"

# ── Cas B : clé privée → refusé ──────────────────────────────────────────────
step "B. Secret / clé privée (doit être REFUSÉ)"
NIDAN_LOG=info SDL_VIDEODRIVER=dummy \
  NIDAN_TEST_CLIPBOARD="-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAA==
-----END OPENSSH PRIVATE KEY-----" \
  timeout 7 ./target/debug/nidan-client --config "$WORK/client.toml" \
    --direct 127.0.0.1:$SERVER_PORT > "$WORK/client_secret.log" 2>&1
sleep 1
kill $SRV 2>/dev/null; SRV=""
ok "session B terminée"

# ── Vérifications ────────────────────────────────────────────────────────────
step "Résultats"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/server.log"        > "$WORK/server.clean"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/client_ok.log"     > "$WORK/client_ok.clean"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/client_secret.log" > "$WORK/client_secret.clean"

PASS=0; FAIL=0
chk(){ if [ "$1" = "0" ]; then ok "$2"; PASS=$((PASS+1)); else ko "$2"; FAIL=$((FAIL+1)); fi; }

# Le filtre est-il actif ?
grep -q "filtre presse-papier actif" "$WORK/server.clean"
chk $? "Serveur : filtre presse-papier actif sur le canal de contrôle"

# Chiffrement E2E avant envoi (cas A)
grep -q "chiffrement E2E actif" "$WORK/client_ok.clean"
chk $? "Client : chiffrement E2E actif avant envoi du presse-papier"

# Cas A : le client a envoyé, le serveur a accepté
grep -q "presse-papier de test envoyé" "$WORK/client_ok.clean"
chk $? "A : client a envoyé le presse-papier anodin"
grep -q "presse-papier accepté" "$WORK/server.clean"
chk $? "A : serveur a ACCEPTÉ le presse-papier anodin (filtre OK)"

# Cas B : le client a envoyé, le serveur a refusé
grep -q "presse-papier de test envoyé" "$WORK/client_secret.clean"
chk $? "B : client a envoyé la clé privée"
grep -q "presse-papier REFUSÉ" "$WORK/server.clean"
chk $? "B : serveur a REFUSÉ la clé privée (motif de sécurité)"

# Le motif de blocage est bien la clé privée
if grep "presse-papier REFUSÉ" "$WORK/server.clean" | grep -q "PRIVATE KEY"; then
  chk 0 "B : motif de blocage correct (clé privée détectée)"
else
  chk 1 "B : motif de blocage inattendu"
fi

# Anti-régression : le secret ne doit PAS apparaître comme accepté
ACCEPTED=$(grep -c "presse-papier accepté" "$WORK/server.clean" 2>/dev/null | head -1)
ACCEPTED=${ACCEPTED:-0}
if [ "$ACCEPTED" -eq 1 ] 2>/dev/null; then
  chk 0 "Exactement 1 transfert accepté (l'anodin), pas le secret"
else
  chk 1 "Nombre de transferts acceptés inattendu ($ACCEPTED)"
fi

echo ""
if [ "$FAIL" = "0" ]; then
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
  echo -e "${G}  ✓✓✓ PHASE 15 VALIDÉE — canal presse-papier + filtrage${NC}"
  echo -e "${G}  Texte anodin  → transmis chiffré → ACCEPTÉ${NC}"
  echo -e "${G}  Clé privée    → transmise chiffrée → REFUSÉE par le filtre${NC}"
  echo -e "${G}  Le presse-papier transite E2E et la politique tranche.${NC}"
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
else
  echo -e "${Y}Des contrôles ont échoué.${NC}"
  echo "--- serveur (filtrage) ---"
  grep -iE "filtre|accepté|REFUSÉ" "$WORK/server.clean" | tail -6
fi
echo ""
info "Logs dans : $WORK/"
echo "Résultat : $PASS réussis, $FAIL échoués"
[ "$KEEP" = "0" ] && rm -f "$WORK"/*.clean
[ "$FAIL" = "0" ] && exit 0 || exit 1
