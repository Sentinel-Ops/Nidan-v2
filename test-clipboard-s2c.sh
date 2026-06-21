#!/usr/bin/env bash
# =============================================================================
# NIDAN — Test de la Phase 16 : presse-papier serveur → client (sens retour)
# =============================================================================
# Prouve que le presse-papier de la VM remonte vers le client, chiffré E2E avec
# un nonce isolé du sens aller, et filtré par la politique avant émission.
#
#   A. Tests unitaires crypto : encrypt_dir sépare les nonces des deux sens
#   B. Retour autorisé : la VM envoie un texte → client le REÇOIT
#   C. Retour refusé   : la VM envoie un secret → bloqué AVANT émission
#
# Le serveur utilise le hook NIDAN_TEST_CLIPBOARD_S2C pour émettre vers le
# client dès l'ouverture du canal de contrôle (pas de capture X réelle).
#
# Usage : ./test-clipboard-s2c.sh [--deps] [--keep-logs]
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

WORK=/tmp/nidan-s2c-test
mkdir -p "$WORK/certs"
DISPLAY_NUM=109; SERVER_PORT=7486

SRV=""; XV=""
cleanup(){
  [ -n "$SRV" ]&&kill $SRV 2>/dev/null
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f "nidan-server" 2>/dev/null
  pkill -f xclock 2>/dev/null
}
trap cleanup EXIT INT TERM

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  NIDAN — Test Phase 16 : presse-papier serveur → client"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [ "$INSTALL_DEPS" = "1" ]; then
  step "Dépendances"
  sudo apt-get install -y xvfb x11-apps libxcb-xtest0-dev libsdl2-dev 2>&1 | tail -1
fi

PASS=0; FAIL=0
chk(){ if [ "$1" = "0" ]; then ok "$2"; PASS=$((PASS+1)); else ko "$2"; FAIL=$((FAIL+1)); fi; }

# =============================================================================
#  A. Tests unitaires crypto (séparation des nonces par direction)
# =============================================================================
step "A. Tests unitaires crypto (encrypt_dir)"
UNIT_OUT="$WORK/unit.log"
cargo test -p nidan-common crypto 2>&1 | tee "$UNIT_OUT" \
  | grep -E "test_encrypt_dir|test result" || true
grep -q "test_encrypt_dir_separates_nonces ... ok" "$UNIT_OUT"
chk $? "encrypt_dir sépare les nonces des deux sens (anti-réutilisation)"
grep -q "test_encrypt_dir_roundtrip ... ok" "$UNIT_OUT"
chk $? "encrypt_dir : chiffrement/déchiffrement round-trip"

# =============================================================================
#  Compilation + PKI + configs
# =============================================================================
step "Compilation + PKI"
cargo build -p nidan-server --features full 2>&1 | tail -1
cargo build -p nidan-client --features openh264 2>&1 | tail -1
[ -f "$WORK/certs/ca.crt" ] || bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1
ok "binaires + PKI prêts"

# Politique : s2c autorisé, mais clés privées bloquées dans les DEUX sens
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
allow_server_to_client = true
max_size_bytes = 65536
blocked_patterns = ["-----BEGIN [A-Z ]*PRIVATE KEY-----"]
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
window_title = "s2c"
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

run_session() { # $1 = contenu s2c, $2 = suffixe log
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f nidan 2>/dev/null; sleep 1
  Xvfb :$DISPLAY_NUM -screen 0 1280x720x24 >/dev/null 2>&1 &
  sleep 2
  command -v xclock >/dev/null && DISPLAY=:$DISPLAY_NUM xclock >/dev/null 2>&1 &
  sleep 1
  NIDAN_SERVER_CONFIG="$WORK/server.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
    NIDAN_TEST_CLIPBOARD_S2C="$1" \
    ./target/debug/nidan-server > "$WORK/server_$2.log" 2>&1 &
  SRV=$!
  sleep 3
  NIDAN_LOG=info SDL_VIDEODRIVER=dummy timeout 9 ./target/debug/nidan-client \
    --config "$WORK/client.toml" --direct 127.0.0.1:$SERVER_PORT > "$WORK/client_$2.log" 2>&1
  sleep 1
  kill $SRV 2>/dev/null; SRV=""
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f xclock 2>/dev/null
}

# =============================================================================
#  B. Retour autorisé : la VM envoie un texte → client le reçoit
# =============================================================================
step "B. Retour autorisé (texte de la VM → client)"
run_session "presse-papier venant de la VM distante" "ok"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/server_ok.log"  > "$WORK/server_ok.clean"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/client_ok.log"  > "$WORK/client_ok.clean"

grep -q "presse-papier serveur→client accepté" "$WORK/server_ok.clean"
chk $? "Serveur : presse-papier retour accepté par le filtre"
grep -q "presse-papier serveur→client émis" "$WORK/server_ok.clean"
chk $? "Serveur : presse-papier retour émis vers le client"
grep -q "presse-papier reçu du serveur" "$WORK/client_ok.clean"
chk $? "Client : presse-papier serveur→client REÇU"

# =============================================================================
#  C. Retour refusé : la VM envoie un secret → bloqué avant émission
# =============================================================================
step "C. Retour refusé (secret de la VM → bloqué)"
run_session "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAA
-----END OPENSSH PRIVATE KEY-----" "sec"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/server_sec.log" > "$WORK/server_sec.clean"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/client_sec.log" > "$WORK/client_sec.clean"

grep -q "serveur→client REFUSÉ" "$WORK/server_sec.clean"
chk $? "Serveur : secret retour REFUSÉ par le filtre (non émis)"
# Le client ne doit RIEN recevoir dans ce cas
if grep -q "presse-papier reçu du serveur" "$WORK/client_sec.clean"; then
  chk 1 "Client : a reçu le secret (FUITE — filtre retour contourné !)"
else
  chk 0 "Client : n'a RIEN reçu (secret bloqué côté serveur)"
fi

# =============================================================================
#  Bilan
# =============================================================================
echo ""
if [ "$FAIL" = "0" ]; then
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
  echo -e "${G}  ✓✓✓ PHASE 16 VALIDÉE — presse-papier serveur → client${NC}"
  echo -e "${G}  Texte de la VM  → filtré → chiffré (nonce isolé) → REÇU${NC}"
  echo -e "${G}  Secret de la VM → REFUSÉ avant émission (pas de fuite)${NC}"
  echo -e "${G}  Le presse-papier est bidirectionnel et filtré dans les 2 sens.${NC}"
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
else
  echo -e "${Y}Des contrôles ont échoué.${NC}"
  echo "--- serveur (B) ---"; grep -iE "serveur→client" "$WORK/server_ok.clean"  | tail -4
  echo "--- serveur (C) ---"; grep -iE "serveur→client|REFUSÉ" "$WORK/server_sec.clean" | tail -4
fi
echo ""
info "Logs dans : $WORK/"
echo "Résultat : $PASS réussis, $FAIL échoués"
[ "$KEEP" = "0" ] && rm -f "$WORK"/*.clean
[ "$FAIL" = "0" ] && exit 0 || exit 1
