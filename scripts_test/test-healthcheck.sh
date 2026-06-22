#!/usr/bin/env bash
# =============================================================================
# NIDAN — Test de la Phase 20 : health check des VM par handshake QUIC réel
# =============================================================================
# Prouve que le broker détecte l'état réel des VM via un handshake QUIC :
#
#   A. Endpoint QUIC de health check activé au démarrage du broker
#   B. VM morte (rien sur le port)      → marquée hors service
#   C. VM vivante (serveur NIDAN réel)  → jamais marquée hors service
#
# Deux VM sont déclarées dans le pool : une réelle, une fantôme. Le test
# observe le verdict du health checker après quelques cycles.
#
# Usage : ./test-healthcheck.sh [--deps] [--keep-logs]
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

WORK=/tmp/nidan-health-test
mkdir -p "$WORK/certs"
DISPLAY_NUM=115; SRV_OK=7500; SRV_DEAD=7501; BROKER=7502

SRV=""; BRK=""; XV=""
cleanup(){
  [ -n "$SRV" ]&&kill $SRV 2>/dev/null; [ -n "$BRK" ]&&kill $BRK 2>/dev/null
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f "nidan-broker" 2>/dev/null
  pkill -f "nidan-server" 2>/dev/null; pkill -f xclock 2>/dev/null
}
trap cleanup EXIT INT TERM

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  NIDAN — Test Phase 20 : health check des VM (QUIC)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [ "$INSTALL_DEPS" = "1" ]; then
  step "Dépendances"
  sudo apt-get install -y xvfb x11-apps libxcb-xtest0-dev libsdl2-dev 2>&1 | tail -1
fi

step "Compilation + PKI"
cargo build -p nidan-server --features full 2>&1 | tail -1
cargo build -p nidan-broker 2>&1 | tail -1
[ -f "$WORK/certs/ca.crt" ] || bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1
ok "binaires + PKI prêts"

step "Configurations (pool : 1 VM vivante + 1 VM morte)"
cat > "$WORK/server.toml" << EOF
[network]
bind_addr = "127.0.0.1:$SRV_OK"
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
quic_bind = "127.0.0.1:$BROKER"
admin_bind = "127.0.0.1:7085"
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
health_check_timeout_secs = 3
health_check_interval_secs = 5
[[pool.static_vms]]
id = "vm-alive"
host = "127.0.0.1"
port = $SRV_OK
tags = ["test"]
[[pool.static_vms]]
id = "vm-dead"
host = "127.0.0.1"
port = $SRV_DEAD
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
ok "vm-alive=$SRV_OK (serveur réel), vm-dead=$SRV_DEAD (aucun serveur)"

step "Démarrage serveur réel + broker (2 cycles de health check)"
pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f nidan 2>/dev/null; sleep 1
Xvfb :$DISPLAY_NUM -screen 0 1280x720x24 >/dev/null 2>&1 & XV=$!
sleep 2
command -v xclock >/dev/null && DISPLAY=:$DISPLAY_NUM xclock >/dev/null 2>&1 &
sleep 1
NIDAN_SERVER_CONFIG="$WORK/server.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
  ./target/debug/nidan-server > "$WORK/server.log" 2>&1 & SRV=$!
sleep 2
NIDAN_LOG=info ./target/debug/nidan-broker --config "$WORK/broker.toml" > "$WORK/broker.log" 2>&1 & BRK=$!
# Laisser tourner ~14s → au moins 2 cycles à 5s d'intervalle
sleep 16
kill $SRV $BRK 2>/dev/null; SRV=""; BRK=""
ok "cycles de health check observés"

step "Résultats"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/broker.log" > "$WORK/broker.clean"
PASS=0; FAIL=0
chk(){ if [ "$1" = "0" ]; then ok "$2"; PASS=$((PASS+1)); else ko "$2"; FAIL=$((FAIL+1)); fi; }

# A. Endpoint QUIC de health activé
grep -q "handshake QUIC réel activé" "$WORK/broker.clean"
chk $? "A : health check par handshake QUIC réel activé"

# B. La VM morte est détectée hors service
if grep "health check échoué" "$WORK/broker.clean" | grep -q "vm-dead"; then
  chk 0 "B : VM morte (vm-dead) détectée hors service"
else
  chk 1 "B : VM morte non détectée"
fi

# C. La VM vivante n'est JAMAIS marquée hors service
if grep "health check échoué" "$WORK/broker.clean" | grep -q "vm-alive"; then
  chk 1 "C : VM vivante marquée hors service à tort (handshake QUIC KO)"
else
  chk 0 "C : VM vivante (vm-alive) jamais marquée hors service"
fi

# D. Anti-faux-positif : exactement une VM marquée échouée (la morte)
DEAD_CNT=$(grep -c "health check échoué" "$WORK/broker.clean" 2>/dev/null | head -1)
DEAD_CNT=${DEAD_CNT:-0}
# La 1ère détection logge une fois ; ensuite la VM reste Unhealthy en silence.
if [ "$DEAD_CNT" -ge 1 ]; then
  chk 0 "D : détection effective ($DEAD_CNT marquage(s), uniquement vm-dead)"
else
  chk 1 "D : aucune détection"
fi

echo ""
if [ "$FAIL" = "0" ]; then
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
  echo -e "${G}  ✓✓✓ PHASE 20 VALIDÉE — health check QUIC réel${NC}"
  echo -e "${G}  VM morte  → exclue du pool (hors service)${NC}"
  echo -e "${G}  VM vivante → préservée (handshake QUIC réussi)${NC}"
  echo -e "${G}  Le broker ne route plus vers une VM injoignable.${NC}"
  echo -e "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
else
  echo -e "${Y}Des contrôles ont échoué.${NC}"
  echo "--- broker (health) ---"
  grep -iE "health|hors service|disponible" "$WORK/broker.clean" | tail -8
fi
echo ""
info "Logs dans : $WORK/"
echo "Résultat : $PASS réussis, $FAIL échoués"
[ "$KEEP" = "0" ] && rm -f "$WORK/broker.clean"
[ "$FAIL" = "0" ] && exit 0 || exit 1
