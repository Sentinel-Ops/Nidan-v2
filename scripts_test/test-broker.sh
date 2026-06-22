#!/usr/bin/env bash
# =============================================================================
# NIDAN — Test du flux broker complet : client → broker → serveur
# =============================================================================
# Valide l'architecture de production : le client s'authentifie auprès du
# broker, qui lui attribue une VM du pool, puis le client se connecte
# directement au serveur retourné avec une session chiffrée E2E.
#
# Contrôles :
#   1. Le broker démarre, écoute, charge le pool de VMs
#   2. Le client passe par le broker (pas de --direct)
#   3. Le broker authentifie et attribue une VM
#   4. Le client se connecte au serveur dont l'adresse vient du broker
#   5. La session est chiffrée E2E (le broker ne voit jamais le contenu)
#
# Usage : ./test-broker.sh [--deps] [--keep-logs]
# À lancer depuis la racine du projet NIDAN.
# =============================================================================
set -uo pipefail
G='\033[0;32m'; R='\033[0;31m'; B='\033[0;34m'; Y='\033[1;33m'; NC='\033[0m'
ok(){ echo -e "${G}✓${NC} $1"; }; ko(){ echo -e "${R}✗${NC} $1"; }
info(){ echo -e "${B}▶${NC} $1"; }; step(){ echo -e "\n${B}━━━ $1 ━━━${NC}"; }

INSTALL_DEPS=0; KEEP=0
for a in "$@"; do case "$a" in
  --deps) INSTALL_DEPS=1;; --keep-logs) KEEP=1;; esac; done

[ -f Cargo.toml ] && [ -d nidan-broker ] || { ko "Lance depuis la racine du projet NIDAN."; exit 1; }

WORK=/tmp/nidan-broker-test
DISPLAY_NUM=102; SERVER_PORT=7464; BROKER_PORT=7443
SRV=""; BRK=""; XV=""
cleanup(){ [ -n "$SRV" ]&&kill $SRV 2>/dev/null; [ -n "$BRK" ]&&kill $BRK 2>/dev/null
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f "nidan-broker" 2>/dev/null
  pkill -f xclock 2>/dev/null; pkill -f xeyes 2>/dev/null; }
trap cleanup EXIT INT TERM

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  NIDAN — Test du flux broker (client → broker → serveur)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [ "$INSTALL_DEPS" = "1" ]; then
  step "Dépendances"; sudo apt-get install -y xvfb x11-apps libxcb-xtest0-dev libsdl2-dev 2>&1 | tail -1
fi

step "Compilation"
cargo build -p nidan-server --features full 2>&1 | tail -1
cargo build -p nidan-client --features openh264 2>&1 | tail -1
cargo build -p nidan-broker 2>&1 | tail -1
ok "binaires prêts"

step "Certificats"
mkdir -p "$WORK/certs"
[ -f "$WORK/certs/ca.crt" ] || bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1
ok "PKI prête (ca / server / broker / client)"

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
EOF
cat > "$WORK/broker.toml" << EOF
[network]
quic_bind = "127.0.0.1:$BROKER_PORT"
admin_bind = "127.0.0.1:7081"
session_timeout_secs = 3600
max_sessions = 10
[auth]
enabled_methods = ["mtls"]
session_token_ttl_secs = 3600
[auth.jwt]
secret = "test_secret_minimum_32_characters_long!"
issuer = "nidan-broker-test"
[pool]
min_available = 1
health_check_timeout_secs = 5
health_check_interval_secs = 30
[[pool.static_vms]]
id = "vm-test-01"
host = "127.0.0.1"
port = $SERVER_PORT
tags = ["test"]
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
max_fps = 10
target_bitrate_kbps = 0
hardware_decode = false
decode_buffer_size = 4
[display]
fullscreen = false
seamless = false
scaling = "fit"
window_title = "NIDAN broker test"
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
ok "configs écrites (client en mode broker, pas --direct)"

step "Démarrage de la chaîne"
pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; sleep 1
Xvfb :$DISPLAY_NUM -screen 0 1280x720x24 >/dev/null 2>&1 & XV=$!
sleep 2
command -v xclock >/dev/null && DISPLAY=:$DISPLAY_NUM xclock >/dev/null 2>&1 &
command -v xeyes >/dev/null && DISPLAY=:$DISPLAY_NUM xeyes >/dev/null 2>&1 &
sleep 1
NIDAN_SERVER_CONFIG="$WORK/server.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
  ./target/debug/nidan-server > "$WORK/server.log" 2>&1 & SRV=$!
sleep 2
NIDAN_LOG=info ./target/debug/nidan-broker --config "$WORK/broker.toml" > "$WORK/broker.log" 2>&1 & BRK=$!
sleep 3
ok "serveur (VM) + broker démarrés"

step "Session client via broker (12 s)"
NIDAN_LOG=info SDL_VIDEODRIVER=dummy \
  timeout 12 ./target/debug/nidan-client --config "$WORK/client.toml" > "$WORK/client.log" 2>&1
sleep 1
kill $SRV $BRK 2>/dev/null
ok "session terminée"

# ── Vérifications ─────────────────────────────────────────────────────────────
step "Résultats"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/broker.log" > "$WORK/broker.clean" 2>/dev/null
sed 's/\x1b\[[0-9;]*m//g' "$WORK/client.log" > "$WORK/client.clean" 2>/dev/null
PASS=0; FAIL=0
chk(){ if [ "$1" = "0" ]; then ok "$2"; PASS=$((PASS+1)); else ko "$2"; FAIL=$((FAIL+1)); fi; }

grep -q "broker QUIC en écoute" "$WORK/broker.clean"; chk $? "Broker : QUIC en écoute, pool de VMs chargé"
grep -q "connexion au broker" "$WORK/client.clean"; chk $? "Client : passe par le broker (pas --direct)"
grep -q "VM assignée" "$WORK/broker.clean"; chk $? "Broker : authentifie et attribue une VM du pool"
grep -q "VM attribuée, session autorisée" "$WORK/client.clean"; chk $? "Client : reçoit l'adresse VM du broker"
grep -q "connexion QUIC serveur établie" "$WORK/client.clean"; chk $? "Client : se connecte au serveur retourné par le broker"
grep -q "chiffrement E2E actif" "$WORK/client.clean"; chk $? "Session : chiffrée E2E (X25519 + ChaCha20-Poly1305)"
# Le broker ne doit voir AUCUN contenu chiffré (opacité)
if grep -qiE "chiffrement|frame|X25519|vidéo" "$WORK/broker.clean"; then
  chk 1 "Broker : opaque au contenu (aucune trace crypto)"
else
  chk 0 "Broker : opaque au contenu (aucune trace crypto/frame)"
fi

echo ""
if [ "$FAIL" = "0" ]; then
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
  echo -e "${G}  ✓✓✓ FLUX BROKER VALIDÉ DE BOUT EN BOUT${NC}"
  echo -e "${G}  client → broker (auth + VM) → serveur (session E2E)${NC}"
  echo -e "${G}  Le broker oriente mais ne voit jamais le contenu.${NC}"
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
else
  echo -e "${Y}Des contrôles ont échoué. Logs :${NC}"
  echo "--- broker ---"; grep -iE "écoute|VM|auth|error" "$WORK/broker.clean" | tail -6
  echo "--- client ---"; grep -iE "broker|serveur|E2E|error" "$WORK/client.clean" | tail -6
fi
echo ""
info "Logs : $WORK/broker.log  /  $WORK/client.log  /  $WORK/server.log"
echo "Résultat : $PASS réussis, $FAIL échoués"
[ "$KEEP" = "0" ] && rm -f "$WORK/broker.clean" "$WORK/client.clean"
[ "$FAIL" = "0" ] && exit 0 || exit 1
