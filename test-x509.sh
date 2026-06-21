#!/usr/bin/env bash
# =============================================================================
# NIDAN — Test de la Phase 13 : parsing X.509 réel du DN client (mTLS)
# =============================================================================
# Prouve que le broker extrait la VRAIE identité du certificat client présenté,
# et non plus le DN figé du stub ("...O=Example...").
#
#   A. Tests unitaires du module mtls (DN, CN, DER invalide, anti-stub)
#   B. Preuve statique : le code ne contient plus le DN figé "Example"
#   C. Test réel  : connexion client → broker, le broker extrait CN=nidan-client
#   D. Cohérence  : le DN extrait par le broker == le DN lu par openssl
#
# Usage : ./test-x509.sh [--deps] [--keep-logs]
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
command -v cargo >/dev/null 2>&1 || { ko "cargo introuvable."; exit 1; }

WORK=/tmp/nidan-x509-test
mkdir -p "$WORK/certs"
DISPLAY_NUM=106; SERVER_PORT=7470; BROKER_PORT=7449
JWT_SECRET="test_secret_minimum_32_characters_long!"
EXPECTED_CN="nidan-client"   # CN attendu dans le cert client de la PKI
STUB_MARKER="Example"        # marqueur de l'ANCIEN stub — ne doit JAMAIS apparaître

SRV=""; BRK=""; XV=""
cleanup(){
  [ -n "$SRV" ]&&kill $SRV 2>/dev/null; [ -n "$BRK" ]&&kill $BRK 2>/dev/null
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f "nidan-broker" 2>/dev/null
  pkill -f xclock 2>/dev/null
}
trap cleanup EXIT INT TERM

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  NIDAN — Test Phase 13 : parsing X.509 réel du DN client"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [ "$INSTALL_DEPS" = "1" ]; then
  step "Dépendances"
  sudo apt-get install -y xvfb x11-apps libxcb-xtest0-dev libsdl2-dev openssl 2>&1 | tail -1
fi

PASS=0; FAIL=0
chk(){ if [ "$1" = "0" ]; then ok "$2"; PASS=$((PASS+1)); else ko "$2"; FAIL=$((FAIL+1)); fi; }

# =============================================================================
#  A. Tests unitaires du module mtls
# =============================================================================
step "A. Tests unitaires (auth::mtls)"
UNIT_OUT="$WORK/unit.log"
cargo test -p nidan-broker mtls 2>&1 | tee "$UNIT_OUT" \
  | grep -E "test auth::mtls|test result" || true
UNIT_PASS=$(grep -oE "result: ok\. [0-9]+ passed" "$UNIT_OUT" | grep -oE "[0-9]+" | head -1)
UNIT_PASS=${UNIT_PASS:-0}
if grep -q "test result: ok" "$UNIT_OUT" && [ "$UNIT_PASS" -ge 4 ]; then
  chk 0 "$UNIT_PASS tests unitaires mtls (DN, CN, DER invalide, anti-stub)"
else
  chk 1 "tests unitaires mtls en échec"
fi
# Vérifie spécifiquement la présence du test anti-stub
grep -q "test_distinct_certs_distinct_identities ... ok" "$UNIT_OUT"
chk $? "test anti-stub présent (2 certs distincts → 2 identités distinctes)"

# =============================================================================
#  B. Preuve statique : plus de DN figé dans le code
# =============================================================================
step "B. Preuve statique (le stub a disparu du code)"
if grep -rq "$STUB_MARKER" nidan-broker/src/auth/mtls.rs; then
  chk 1 "le DN figé '$STUB_MARKER' est ENCORE dans mtls.rs (stub non retiré)"
else
  chk 0 "aucun DN figé '$STUB_MARKER' dans mtls.rs (stub retiré)"
fi
# Le parsing réel est bien câblé
grep -q "parse_x509_certificate" nidan-broker/src/auth/mtls.rs
chk $? "parse_x509_certificate utilisé (parsing X.509 réel)"

# =============================================================================
#  Compilation des binaires + PKI
# =============================================================================
step "Compilation + PKI"
cargo build -p nidan-server --features full 2>&1 | tail -1
cargo build -p nidan-client --features openh264 2>&1 | tail -1
cargo build -p nidan-broker 2>&1 | tail -1
[ -f "$WORK/certs/ca.crt" ] || bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1
ok "binaires + PKI prêts"

# DN attendu, lu indépendamment par openssl (source de vérité)
OPENSSL_SUBJECT=$(openssl x509 -in "$WORK/certs/client.crt" -noout -subject 2>/dev/null)
info "Cert client (openssl) : $OPENSSL_SUBJECT"

# =============================================================================
#  Configs
# =============================================================================
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
admin_bind = "127.0.0.1:7084"
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
id = "vm-x509-01"
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
window_title = "x509 test"
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

# =============================================================================
#  C + D. Test réel : le broker extrait le vrai DN
# =============================================================================
step "C. Test réel (client → broker, extraction du DN)"
pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; sleep 1
Xvfb :$DISPLAY_NUM -screen 0 1280x720x24 >/dev/null 2>&1 & XV=$!
sleep 2
command -v xclock >/dev/null && DISPLAY=:$DISPLAY_NUM xclock >/dev/null 2>&1 &
sleep 1
NIDAN_SERVER_CONFIG="$WORK/server.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
  ./target/debug/nidan-server > "$WORK/server.log" 2>&1 & SRV=$!
sleep 2
NIDAN_LOG=info ./target/debug/nidan-broker --config "$WORK/broker.toml" > "$WORK/broker.log" 2>&1 & BRK=$!
sleep 3
NIDAN_LOG=info SDL_VIDEODRIVER=dummy \
  timeout 10 ./target/debug/nidan-client --config "$WORK/client.toml" > "$WORK/client.log" 2>&1
sleep 1
kill $SRV $BRK 2>/dev/null; SRV=""; BRK=""

sed 's/\x1b\[[0-9;]*m//g' "$WORK/broker.log" > "$WORK/broker.clean" 2>/dev/null

# Le broker a-t-il extrait une identité ?
grep -q "identité mTLS extraite" "$WORK/broker.clean"
chk $? "Broker : une identité mTLS a été extraite du certificat"

# Cette identité contient-elle le VRAI CN ?
if grep -q "CN=$EXPECTED_CN" "$WORK/broker.clean"; then
  chk 0 "Broker : DN réel extrait (CN=$EXPECTED_CN)"
else
  chk 1 "Broker : CN=$EXPECTED_CN absent du log (parsing réel KO)"
fi

# Surtout PAS le DN du stub
if grep -q "$STUB_MARKER" "$WORK/broker.clean"; then
  chk 1 "Broker : DN du STUB ('$STUB_MARKER') détecté — régression !"
else
  chk 0 "Broker : aucun DN de stub ('$STUB_MARKER') dans le log"
fi

# L'identité réelle se propage à l'auth/session
grep -qE "user=$EXPECTED_CN|user_id=$EXPECTED_CN" "$WORK/broker.clean"
chk $? "Broker : l'identité réelle se propage à l'auth et à la session"

step "D. Cohérence broker ↔ openssl"
# Extrait le DN logué par le broker et compare au CN openssl
BROKER_DN=$(grep "identité mTLS extraite" "$WORK/broker.clean" | head -1 | sed -E 's/.*client_dn=//')
info "DN extrait par le broker : $BROKER_DN"
if echo "$BROKER_DN" | grep -q "CN=$EXPECTED_CN" && echo "$OPENSSL_SUBJECT" | grep -q "CN = $EXPECTED_CN"; then
  chk 0 "Le DN du broker correspond au certificat lu par openssl"
else
  chk 1 "Incohérence entre le DN du broker et le certificat openssl"
fi

# =============================================================================
#  Bilan
# =============================================================================
echo ""
if [ "$FAIL" = "0" ]; then
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
  echo -e "${G}  ✓✓✓ PHASE 13 VALIDÉE — parsing X.509 réel${NC}"
  echo -e "${G}  Le broker extrait le vrai DN du certificat client.${NC}"
  echo -e "${G}  Identité réelle (CN=$EXPECTED_CN), plus aucun stub.${NC}"
  echo -e "${G}  Imputabilité mTLS effective.${NC}"
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
else
  echo -e "${Y}Des contrôles ont échoué.${NC}"
  echo "--- broker (identité) ---"; grep -iE "identité|client_dn|user=|user_id=" "$WORK/broker.clean" | tail -6
fi
echo ""
info "Logs dans : $WORK/"
echo "Résultat : $PASS réussis, $FAIL échoués"
[ "$KEEP" = "0" ] && rm -f "$WORK/broker.clean"
[ "$FAIL" = "0" ] && exit 0 || exit 1
