#!/usr/bin/env bash
# =============================================================================
# NIDAN — Test de la Phase 12 : vérification du jeton de session (JWT)
# =============================================================================
# Valide que l'autorisation du broker est cryptographiquement contraignante :
#
#   A. Tests unitaires du module session_token (signature, expiration, secret)
#   B. Scénario LÉGITIME : client via broker (JWT valide) → session ACCEPTÉE
#   C. Scénario ATTAQUE  : client --direct sans JWT, serveur strict → REFUSÉE
#
# Le test C est le cœur du patch : il prouve qu'un client qui tente de
# contourner le broker en se connectant directement à une VM est rejeté.
#
# Usage :
#   ./test-jwt.sh            # tout (unitaires + légitime + attaque)
#   ./test-jwt.sh --deps     # installe les dépendances système d'abord
#   ./test-jwt.sh --keep-logs
#
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

WORK=/tmp/nidan-jwt-test
mkdir -p "$WORK/certs"
DISPLAY_NUM=104
BROKER_PORT=7445
SERVER_PORT_OK=7466     # serveur pour le scénario légitime
SERVER_PORT_STRICT=7476 # serveur pour le scénario d'attaque
JWT_SECRET="test_secret_minimum_32_characters_long!"

SRV1=""; SRV2=""; BRK=""; XV=""
cleanup(){
  [ -n "$SRV1" ]&&kill $SRV1 2>/dev/null; [ -n "$SRV2" ]&&kill $SRV2 2>/dev/null
  [ -n "$BRK" ]&&kill $BRK 2>/dev/null
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f "nidan-broker" 2>/dev/null
  pkill -f xclock 2>/dev/null; pkill -f xeyes 2>/dev/null
}
trap cleanup EXIT INT TERM

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  NIDAN — Test Phase 12 : vérification JWT du broker"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [ "$INSTALL_DEPS" = "1" ]; then
  step "Dépendances"
  sudo apt-get install -y xvfb x11-apps libxcb-xtest0-dev libsdl2-dev 2>&1 | tail -1
fi

# =============================================================================
#  A. Tests unitaires du module session_token
# =============================================================================
step "A. Tests unitaires (session_token)"
UNIT_OUT="$WORK/unit.log"
cargo test -p nidan-server session_token 2>&1 | tee "$UNIT_OUT" | grep -E "test session_token|test result" || true
UNIT_PASS=$(grep -oE "result: ok\. [0-9]+ passed" "$UNIT_OUT" | grep -oE "[0-9]+" | head -1)
UNIT_PASS=${UNIT_PASS:-0}
if grep -q "test result: ok" "$UNIT_OUT" && [ "$UNIT_PASS" -ge 4 ]; then
  ok "$UNIT_PASS tests unitaires JWT passés (signature, expiration, secret)"
else
  ko "tests unitaires JWT en échec"
fi

# =============================================================================
#  Compilation des binaires réels
# =============================================================================
step "Compilation des binaires"
cargo build -p nidan-server --features full 2>&1 | tail -1
cargo build -p nidan-client --features openh264 2>&1 | tail -1
cargo build -p nidan-broker 2>&1 | tail -1
ok "binaires prêts"

# PKI
[ -f "$WORK/certs/ca.crt" ] || bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1
ok "PKI prête"

# =============================================================================
#  Génération des configs
# =============================================================================
# Bloc TLS commun
tls_block(){ cat << EOF
[tls]
ca_cert = "$WORK/certs/ca.crt"
cert = "$WORK/certs/$1.crt"
key = "$WORK/certs/$1.key"
EOF
}

# Serveur générique (param: port, display, require_token, secret)
gen_server(){ # $1=port $2=display $3=require $4=secret
  cat << EOF
[network]
bind_addr = "127.0.0.1:$1"
session_timeout_secs = 3600
max_connections = 4
$(tls_block server)
[capture]
display_number = $2
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
jwt_secret = "$4"
require_session_token = $3
EOF
}

# Broker (VM statique → serveur légitime)
cat > "$WORK/broker.toml" << EOF
[network]
quic_bind = "127.0.0.1:$BROKER_PORT"
admin_bind = "127.0.0.1:7082"
session_timeout_secs = 3600
max_sessions = 10
[auth]
enabled_methods = ["mtls"]
session_token_ttl_secs = 3600
[auth.jwt]
secret = "$JWT_SECRET"
issuer = "nidan-broker-test"
[pool]
min_available = 1
health_check_timeout_secs = 5
health_check_interval_secs = 30
[[pool.static_vms]]
id = "vm-jwt-01"
host = "127.0.0.1"
port = $SERVER_PORT_OK
tags = ["test"]
$(tls_block broker)
[security]
rate_limit_auth = true
max_auth_attempts = 5
ban_duration_secs = 300
audit_all_auth = true
[admin]
metrics_enabled = true
EOF

# Client mode broker
cat > "$WORK/client-broker.toml" << EOF
[network]
broker_addr = "127.0.0.1:$BROKER_PORT"
connect_timeout_secs = 10
auto_reconnect = false
reconnect_delay_secs = 3
$(tls_block client)
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
window_title = "jwt broker"
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

# Client mode direct (sans broker, sans token)
cat > "$WORK/client-direct.toml" << EOF
[network]
broker_addr = "127.0.0.1:$BROKER_PORT"
connect_timeout_secs = 10
auto_reconnect = false
reconnect_delay_secs = 3
$(tls_block client)
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
window_title = "jwt direct"
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

# Serveur légitime : require_session_token = true, secret partagé avec le broker
gen_server "$SERVER_PORT_OK" "$DISPLAY_NUM" "true" "$JWT_SECRET" > "$WORK/server-ok.toml"
# Serveur strict (scénario attaque) : require = true aussi
gen_server "$SERVER_PORT_STRICT" "$DISPLAY_NUM" "true" "$JWT_SECRET" > "$WORK/server-strict.toml"

# Écran virtuel partagé
pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; sleep 1
Xvfb :$DISPLAY_NUM -screen 0 1280x720x24 >/dev/null 2>&1 & XV=$!
sleep 2
command -v xclock >/dev/null && DISPLAY=:$DISPLAY_NUM xclock >/dev/null 2>&1 &
command -v xeyes >/dev/null && DISPLAY=:$DISPLAY_NUM xeyes >/dev/null 2>&1 &
sleep 1

PASS=0; FAIL=0
chk(){ if [ "$1" = "0" ]; then ok "$2"; PASS=$((PASS+1)); else ko "$2"; FAIL=$((FAIL+1)); fi; }
[ "$UNIT_PASS" -ge 4 ] && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

# =============================================================================
#  B. Scénario LÉGITIME : client via broker (JWT valide) → ACCEPTÉ
# =============================================================================
step "B. Scénario légitime (client → broker → serveur)"
NIDAN_SERVER_CONFIG="$WORK/server-ok.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
  ./target/debug/nidan-server > "$WORK/server-ok.log" 2>&1 & SRV1=$!
sleep 2
NIDAN_LOG=info ./target/debug/nidan-broker --config "$WORK/broker.toml" > "$WORK/broker.log" 2>&1 & BRK=$!
sleep 3
NIDAN_LOG=info SDL_VIDEODRIVER=dummy \
  timeout 10 ./target/debug/nidan-client --config "$WORK/client-broker.toml" > "$WORK/client-broker.log" 2>&1
sleep 1
kill $SRV1 $BRK 2>/dev/null; SRV1=""; BRK=""

sed 's/\x1b\[[0-9;]*m//g' "$WORK/server-ok.log" > "$WORK/server-ok.clean"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/client-broker.log" > "$WORK/client-broker.clean"

grep -q "jeton de session broker validé" "$WORK/server-ok.clean"; chk $? "Serveur : jeton broker VALIDÉ"
grep -q "session démarrée" "$WORK/server-ok.clean"; chk $? "Serveur : session démarrée"
grep -q "frame reçue" "$WORK/client-broker.clean"; chk $? "Client légitime : reçoit des frames (session active)"

# =============================================================================
#  C. Scénario ATTAQUE : client --direct sans JWT → REFUSÉ
# =============================================================================
step "C. Scénario attaque (--direct, sans jeton broker)"
NIDAN_SERVER_CONFIG="$WORK/server-strict.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
  ./target/debug/nidan-server > "$WORK/server-strict.log" 2>&1 & SRV2=$!
sleep 2
NIDAN_LOG=info SDL_VIDEODRIVER=dummy \
  timeout 8 ./target/debug/nidan-client \
    --config "$WORK/client-direct.toml" --direct 127.0.0.1:$SERVER_PORT_STRICT \
    > "$WORK/client-direct.log" 2>&1
sleep 1
kill $SRV2 2>/dev/null; SRV2=""

sed 's/\x1b\[[0-9;]*m//g' "$WORK/server-strict.log" > "$WORK/server-strict.clean"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/client-direct.log" > "$WORK/client-direct.clean"

grep -q "jeton de session refusé" "$WORK/server-strict.clean"; chk $? "Serveur : jeton absent → session REJETÉE"
# Le serveur ne doit PAS avoir démarré de session pour ce client
DIRECT_FRAMES=$(grep -c "frame reçue" "$WORK/client-direct.clean" 2>/dev/null | head -1)
DIRECT_FRAMES=${DIRECT_FRAMES:-0}
if [ "$DIRECT_FRAMES" -eq 0 ] 2>/dev/null; then
  chk 0 "Client attaquant : 0 frame (bypass bloqué)"
else
  chk 1 "Client attaquant : a reçu $DIRECT_FRAMES frames (FUITE — bypass non bloqué !)"
fi
# Le serveur strict ne doit PAS avoir validé de jeton
if grep -q "jeton de session broker validé" "$WORK/server-strict.clean"; then
  chk 1 "Serveur strict : aucun jeton validé à tort"
else
  chk 0 "Serveur strict : aucun jeton accepté sans broker"
fi

# =============================================================================
#  Bilan
# =============================================================================
echo ""
if [ "$FAIL" = "0" ]; then
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
  echo -e "${G}  ✓✓✓ PHASE 12 VALIDÉE — autorisation broker contraignante${NC}"
  echo -e "${G}  JWT valide (via broker)  → session acceptée${NC}"
  echo -e "${G}  Sans JWT (--direct)      → session refusée${NC}"
  echo -e "${G}  Le contournement du broker est bloqué cryptographiquement.${NC}"
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
else
  echo -e "${Y}Des contrôles ont échoué.${NC}"
  echo "--- serveur légitime ---"; grep -iE "jeton|session démarrée|error" "$WORK/server-ok.clean" | tail -4
  echo "--- serveur strict ---";  grep -iE "jeton|refusé|error" "$WORK/server-strict.clean" | tail -4
fi
echo ""
info "Logs dans : $WORK/"
echo "Résultat : $PASS réussis, $FAIL échoués"

if [ "$KEEP" = "0" ]; then rm -f "$WORK"/*.clean; fi
[ "$FAIL" = "0" ] && exit 0 || exit 1
