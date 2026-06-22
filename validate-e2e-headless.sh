#!/usr/bin/env bash
# =============================================================================
# NIDAN — Scénario de validation de bout en bout (serveur Linux SANS écran)
# =============================================================================
# Valide la pile NIDAN complète sur un serveur Linux headless (sans interface
# graphique physique). L'écran de la « VM » est fourni par Xvfb (framebuffer
# virtuel) ; aucune carte graphique ni session de bureau n'est requise.
#
# Pile validée, dans l'ordre opérationnel réel :
#   1. PKI       — autorité + certificats serveur / broker / client
#   2. Écran     — Xvfb (framebuffer virtuel, headless) + contenu de test
#   3. Serveur   — capture X → encode H.264 → QUIC, écoute mTLS
#   4. Broker    — pool de VM, auth mTLS, attribution, health check QUIC
#   5. Client    — broker → VM, session E2E (X25519 + ChaCha20-Poly1305)
#   6. Contrôles — identité mTLS réelle, jeton de session, presse-papier filtré
#
# Pré-requis (Debian/Ubuntu) :
#   apt-get install -y xvfb x11-apps openssl libxcb-xtest0-dev libsdl2-dev
#   + chaîne Rust (rustup) pour compiler, OU binaires déjà compilés
#
# Usage :
#   ./validate-e2e-headless.sh [--deps] [--build] [--keep-logs] [--port-base N]
#     --deps        installe les paquets système requis
#     --build       (re)compile serveur/broker/client avant validation
#     --keep-logs   conserve les journaux détaillés
#     --port-base   base de ports (défaut 7600), pour éviter les collisions
#
# Sortie : code 0 si toute la chaîne est validée, 1 sinon.
# À lancer depuis la racine du projet NIDAN.
# =============================================================================
set -uo pipefail
G='\033[0;32m'; R='\033[0;31m'; B='\033[0;34m'; Y='\033[1;33m'; C='\033[0;36m'; NC='\033[0m'
ok(){ echo -e "${G}✓${NC} $1"; }; ko(){ echo -e "${R}✗${NC} $1"; }
info(){ echo -e "${B}▶${NC} $1"; }; step(){ echo -e "\n${C}━━━ $1 ━━━${NC}"; }

INSTALL_DEPS=0; DO_BUILD=0; KEEP=0; PORT_BASE=7600
while [ $# -gt 0 ]; do case "$1" in
  --deps) INSTALL_DEPS=1; shift ;;
  --build) DO_BUILD=1; shift ;;
  --keep-logs) KEEP=1; shift ;;
  --port-base) PORT_BASE="$2"; shift 2 ;;
  *) shift ;;
esac; done

[ -f Cargo.toml ] && [ -d nidan-broker ] || { ko "Lance depuis la racine du projet NIDAN."; exit 1; }

WORK=/tmp/nidan-e2e-headless
mkdir -p "$WORK/certs"
DISPLAY_NUM=120
SERVER_PORT=$PORT_BASE
BROKER_PORT=$((PORT_BASE+1))
ADMIN_PORT=$((PORT_BASE+2))
JWT_SECRET="validation_secret_minimum_32_chars_ok!"

SRV=""; BRK=""; XV=""
cleanup(){
  [ -n "$SRV" ]&&kill $SRV 2>/dev/null; [ -n "$BRK" ]&&kill $BRK 2>/dev/null
  pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f "nidan-broker" 2>/dev/null
  pkill -f "nidan-server" 2>/dev/null; pkill -f "xclock" 2>/dev/null; pkill -f "xeyes" 2>/dev/null
}
trap cleanup EXIT INT TERM

echo "═══════════════════════════════════════════════════════════════"
echo "  NIDAN — Validation de bout en bout (serveur Linux headless)"
echo "  $(date '+%Y-%m-%d %H:%M:%S')  —  hôte : $(hostname)  —  $(uname -sm)"
echo "═══════════════════════════════════════════════════════════════"

PASS=0; FAIL=0
chk(){ if [ "$1" = "0" ]; then ok "$2"; PASS=$((PASS+1)); else ko "$2"; FAIL=$((FAIL+1)); fi; }

# ── 0. Dépendances ───────────────────────────────────────────────────────────
if [ "$INSTALL_DEPS" = "1" ]; then
  step "0. Installation des dépendances système"
  sudo apt-get update -qq 2>/dev/null || true
  sudo apt-get install -y xvfb x11-apps openssl libxcb-xtest0-dev libsdl2-dev xclip 2>&1 | tail -1
fi

step "0. Vérification de l'environnement headless"
info "DISPLAY physique : ${DISPLAY:-<aucun>} (attendu : aucun sur un serveur headless)"
command -v Xvfb >/dev/null && ok "Xvfb présent (framebuffer virtuel disponible)" || { ko "Xvfb manquant — installez xvfb (--deps)"; exit 1; }
command -v openssl >/dev/null && ok "openssl présent" || { ko "openssl manquant"; exit 1; }
command -v cargo >/dev/null && ok "chaîne Rust présente" || info "cargo absent — les binaires doivent déjà être compilés"

# ── 1. Compilation (optionnelle) ─────────────────────────────────────────────
if [ "$DO_BUILD" = "1" ]; then
  step "1. Compilation des composants"
  cargo build -p nidan-server --features full 2>&1 | tail -1
  cargo build -p nidan-client --features full 2>&1 | tail -1
  cargo build -p nidan-broker 2>&1 | tail -1
fi
for bin in nidan-server nidan-broker nidan-client; do
  [ -x "target/debug/$bin" ] || [ -x "target/release/$bin" ] \
    || { ko "binaire $bin introuvable — relancez avec --build"; exit 1; }
done
BIN=target/debug; [ -x target/release/nidan-server ] && BIN=target/release
ok "binaires présents ($BIN)"

# ── 2. PKI ───────────────────────────────────────────────────────────────────
step "2. Infrastructure à clés (PKI mTLS)"
bash scripts/pki-init.sh --out-dir "$WORK/certs" >/dev/null 2>&1
for c in ca server broker client; do
  [ -f "$WORK/certs/$c.crt" ] && [ -f "$WORK/certs/$c.key" ] \
    && info "  $c.crt / $c.key" || { ko "certificat $c manquant"; exit 1; }
done
chk 0 "PKI complète (ca, server, broker, client)"
info "DN client : $(openssl x509 -in "$WORK/certs/client.crt" -noout -subject 2>/dev/null | sed 's/subject=//')"

# ── 3. Écran virtuel headless ────────────────────────────────────────────────
step "3. Écran virtuel (Xvfb — aucune carte graphique requise)"
pkill -f "Xvfb :$DISPLAY_NUM" 2>/dev/null; pkill -f nidan 2>/dev/null; sleep 1
Xvfb :$DISPLAY_NUM -screen 0 1280x720x24 >/dev/null 2>&1 & XV=$!
sleep 2
if DISPLAY=:$DISPLAY_NUM xdpyinfo >/dev/null 2>&1 || kill -0 $XV 2>/dev/null; then
  chk 0 "framebuffer virtuel actif sur :$DISPLAY_NUM (1280x720x24)"
else
  chk 1 "Xvfb n'a pas démarré"
fi
command -v xclock >/dev/null && DISPLAY=:$DISPLAY_NUM xclock >/dev/null 2>&1 &
command -v xeyes >/dev/null && DISPLAY=:$DISPLAY_NUM xeyes >/dev/null 2>&1 &
sleep 1
info "contenu de test affiché (horloge + yeux)"

# ── 4. Configurations ────────────────────────────────────────────────────────
step "4. Configurations (serveur durci : jeton requis, presse-papier filtré)"
cat > "$WORK/server.toml" << EOF
[network]
bind_addr = "127.0.0.1:$SERVER_PORT"
session_timeout_secs = 3600
max_connections = 8
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
max_fps = 15
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
blocked_patterns = ["-----BEGIN [A-Z ]*PRIVATE KEY-----", "\\\\b\\\\d{4}[ -]?\\\\d{4}[ -]?\\\\d{4}[ -]?\\\\d{4}\\\\b"]
audit_transfers = true
EOF

cat > "$WORK/broker.toml" << EOF
[network]
quic_bind = "127.0.0.1:$BROKER_PORT"
admin_bind = "127.0.0.1:$ADMIN_PORT"
session_timeout_secs = 3600
max_sessions = 16
[auth]
enabled_methods = ["mtls"]
session_token_ttl_secs = 3600
[auth.jwt]
secret = "$JWT_SECRET"
issuer = "nidan-broker"
[pool]
min_available = 1
health_check_timeout_secs = 3
health_check_interval_secs = 10
[[pool.static_vms]]
id = "vm-prod-01"
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
max_fps = 15
target_bitrate_kbps = 0
hardware_decode = false
decode_buffer_size = 4
[display]
fullscreen = false
seamless = false
scaling = "fit"
window_title = "NIDAN validation"
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
chk 0 "configurations serveur / broker / client générées"

# ── 5. Démarrage de la pile ──────────────────────────────────────────────────
step "5. Démarrage serveur + broker"
NIDAN_SERVER_CONFIG="$WORK/server.toml" NIDAN_DISPLAY=$DISPLAY_NUM NIDAN_LOG=info \
  "$BIN/nidan-server" > "$WORK/server.log" 2>&1 & SRV=$!
sleep 2
NIDAN_LOG=info "$BIN/nidan-broker" --config "$WORK/broker.toml" > "$WORK/broker.log" 2>&1 & BRK=$!
sleep 3
sed 's/\x1b\[[0-9;]*m//g' "$WORK/server.log" > "$WORK/server.clean"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/broker.log" > "$WORK/broker.clean"
grep -q "serveur NIDAN prêt\|en écoute" "$WORK/server.clean"; chk $? "serveur QUIC en écoute (mTLS, port $SERVER_PORT)"
grep -q "broker QUIC en écoute\|pool" "$WORK/broker.clean"; chk $? "broker en écoute, pool initialisé (port $BROKER_PORT)"
grep -q "handshake QUIC réel activé" "$WORK/broker.clean"; chk $? "health check QUIC actif"

# ── 6. Session client de bout en bout ────────────────────────────────────────
step "6. Session client via broker (avec presse-papier de test)"
NIDAN_LOG=info SDL_VIDEODRIVER=dummy NIDAN_TEST_CLIPBOARD="validation E2E texte anodin" \
  timeout 14 "$BIN/nidan-client" --config "$WORK/client.toml" > "$WORK/client.log" 2>&1
sleep 1
kill $SRV $BRK 2>/dev/null; SRV=""; BRK=""
sed 's/\x1b\[[0-9;]*m//g' "$WORK/client.log" > "$WORK/client.clean"
# rafraîchir les journaux serveur/broker (ils ont continué d'écrire)
sed 's/\x1b\[[0-9;]*m//g' "$WORK/server.log" > "$WORK/server.clean"
sed 's/\x1b\[[0-9;]*m//g' "$WORK/broker.log" > "$WORK/broker.clean"

# ── 7. Contrôles fonctionnels ────────────────────────────────────────────────
step "7. Contrôles de bout en bout"

grep -q "VM attribuée, session autorisée" "$WORK/client.clean"
chk $? "Broker : authentifie le client et attribue une VM du pool"

grep -qE "identité mTLS extraite.*nidan-client|user=nidan-client" "$WORK/broker.clean"
chk $? "Identité mTLS réelle extraite du certificat (CN=nidan-client)"

grep -q "jeton de session broker validé" "$WORK/server.clean"
chk $? "Serveur : jeton de session du broker vérifié (anti-contournement)"

grep -q "connexion QUIC serveur établie" "$WORK/client.clean"
chk $? "Client : connecté directement à la VM retournée par le broker"

grep -q "chiffrement E2E actif" "$WORK/client.clean"
chk $? "Session chiffrée E2E (X25519 + ChaCha20-Poly1305)"

grep -q "frame reçue" "$WORK/client.clean"
chk $? "Flux vidéo reçu et décodé (frames)"

grep -q "presse-papier accepté" "$WORK/server.clean"
chk $? "Presse-papier filtré et accepté (texte anodin)"

# Le broker ne voit jamais le contenu chiffré (opacité)
if grep -qiE "frame|X25519|chiffrement vidéo" "$WORK/broker.clean"; then
  chk 1 "Broker opaque au contenu (trace crypto détectée — anormal)"
else
  chk 0 "Broker opaque au contenu de session (aucune trace crypto/frame)"
fi

# ── Bilan ────────────────────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════════════"
if [ "$FAIL" = "0" ]; then
  echo -e "${G}  ✓✓✓ VALIDATION DE BOUT EN BOUT RÉUSSIE${NC}"
  echo -e "${G}  Pile NIDAN opérationnelle sur serveur Linux headless.${NC}"
  echo -e "${G}  PKI → Xvfb → serveur → broker → client → session E2E.${NC}"
else
  echo -e "${Y}  Validation partielle : $FAIL contrôle(s) en échec.${NC}"
  echo -e "${Y}  Consultez les journaux pour le détail.${NC}"
fi
echo "═══════════════════════════════════════════════════════════════"
echo ""
info "Journaux : $WORK/{server,broker,client}.log"
echo "Résultat : $PASS réussis, $FAIL échoués"
[ "$KEEP" = "0" ] && rm -f "$WORK"/*.clean
[ "$FAIL" = "0" ] && exit 0 || exit 1
