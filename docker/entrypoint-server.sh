#!/bin/bash
# Lance Xvfb sur :100 puis nidan-server
set -e

DISPLAY_NUM="${NIDAN_DISPLAY_NUM:-100}"
DISPLAY=":${DISPLAY_NUM}"

echo "[entrypoint] Démarrage Xvfb sur ${DISPLAY}..."
Xvfb "${DISPLAY}" -screen 0 1920x1080x24 -ac +extension GLX +render -noreset &
XVFB_PID=$!

# Attendre que Xvfb soit prêt
sleep 1

echo "[entrypoint] Démarrage nidan-server..."
DISPLAY="${DISPLAY}" exec /usr/local/bin/nidan-server \
    --config "${NIDAN_CONFIG:-/etc/nidan-server.toml}" \
    "$@"
