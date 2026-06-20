#!/bin/bash
# Session E2E complète, synchrone, avec rapport final
source "$HOME/.cargo/env"
cd /home/claude/nidan
export RESULT=/tmp/e2e_result.txt
: > $RESULT

pkill -f nidan 2>/dev/null; pkill -f "Xvfb :100" 2>/dev/null
sleep 1

# 1. Xvfb
Xvfb :100 -screen 0 1280x720x24 >/dev/null 2>&1 &
XVFB=$!
sleep 2
DISPLAY=:100 xclock >/dev/null 2>&1 &
DISPLAY=:100 xeyes >/dev/null 2>&1 &
sleep 1
echo "Xvfb=$XVFB" >> $RESULT

# 2. Serveur
NIDAN_SERVER_CONFIG=/tmp/nidan-e2e/server.toml NIDAN_DISPLAY=100 NIDAN_LOG=info \
  ./target/debug/nidan-server > /tmp/srvF.log 2>&1 &
SRV=$!
sleep 3
echo "Server=$SRV" >> $RESULT

# 3. Client (10s max)
NIDAN_LOG=info SDL_VIDEODRIVER=dummy \
  timeout 10 ./target/debug/nidan-client \
    --config /tmp/nidan-e2e/client.toml --direct 127.0.0.1:7444 > /tmp/cliF.log 2>&1 &
CLI=$!
sleep 11
echo "Client done" >> $RESULT

# 4. Arrêt propre
kill $SRV $XVFB 2>/dev/null
pkill -f xclock 2>/dev/null; pkill -f xeyes 2>/dev/null
echo "DONE" >> $RESULT
