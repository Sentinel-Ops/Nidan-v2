#!/usr/bin/env bash
# =============================================================================
# NIDAN PKI Init — génération de l'infrastructure à clés publiques complète
# =============================================================================
# Usage :
#   ./scripts/pki-init.sh [--out-dir /etc/nidan/certs] [--days 365]
#
# Génère :
#   ca.crt / ca.key         — Autorité de certification racine NIDAN
#   broker.crt / broker.key — Certificat du broker
#   server.crt / server.key — Certificat du serveur (VM)
#   client.crt / client.key — Certificat client (utilisateur)
#
# Prérequis : openssl >= 1.1.1

set -euo pipefail

# ── Arguments ─────────────────────────────────────────────────────────────────
OUT_DIR="${NIDAN_CERTS_DIR:-./certs}"
DAYS="${NIDAN_CERT_DAYS:-825}"          # 825 jours ≈ 2 ans (max Apple)
CA_CN="${NIDAN_CA_CN:-NIDAN Root CA}"
ORG="${NIDAN_ORG:-NIDAN}"
COUNTRY="${NIDAN_COUNTRY:-FR}"
KEY_SIZE=4096

while [[ $# -gt 0 ]]; do
  case $1 in
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    --days)    DAYS="$2";    shift 2 ;;
    --org)     ORG="$2";     shift 2 ;;
    *) echo "Option inconnue: $1"; exit 1 ;;
  esac
done

mkdir -p "$OUT_DIR"
cd "$OUT_DIR"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  NIDAN PKI Init"
echo "  Répertoire : $OUT_DIR"
echo "  Durée      : $DAYS jours"
echo "  Org        : $ORG"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# ── Fonction utilitaire ────────────────────────────────────────────────────────
gen_cert() {
  local name="$1"
  local cn="$2"
  local ext="$3"   # "ca", "server", "client"

  echo ""
  echo "▶ Génération : $name ($cn)"

  # Clé privée
  openssl genrsa -out "${name}.key" "$KEY_SIZE" 2>/dev/null
  chmod 600 "${name}.key"

  # CSR
  openssl req -new \
    -key "${name}.key" \
    -out "${name}.csr" \
    -subj "/C=${COUNTRY}/O=${ORG}/CN=${cn}" \
    2>/dev/null

  # Certificat selon le type
  case "$ext" in
    ca)
      openssl x509 -req \
        -in "${name}.csr" \
        -signkey "${name}.key" \
        -out "${name}.crt" \
        -days "$DAYS" \
        -extensions v3_ca \
        -extfile <(cat <<EXTEOF
[v3_ca]
basicConstraints       = critical,CA:TRUE
keyUsage               = critical,keyCertSign,cRLSign
subjectKeyIdentifier   = hash
authorityKeyIdentifier = keyid:always,issuer
EXTEOF
        ) 2>/dev/null
      ;;

    server)
      openssl x509 -req \
        -in "${name}.csr" \
        -CA ca.crt -CAkey ca.key -CAcreateserial \
        -out "${name}.crt" \
        -days "$DAYS" \
        -extensions v3_server \
        -extfile <(cat <<EXTEOF
[v3_server]
basicConstraints       = critical,CA:FALSE
keyUsage               = critical,digitalSignature,keyEncipherment
extendedKeyUsage       = serverAuth
subjectAltName         = DNS:localhost,DNS:nidan-server,DNS:nidan-broker,IP:127.0.0.1
subjectKeyIdentifier   = hash
authorityKeyIdentifier = keyid,issuer
EXTEOF
        ) 2>/dev/null
      ;;

    client)
      openssl x509 -req \
        -in "${name}.csr" \
        -CA ca.crt -CAkey ca.key -CAcreateserial \
        -out "${name}.crt" \
        -days "$DAYS" \
        -extensions v3_client \
        -extfile <(cat <<EXTEOF
[v3_client]
basicConstraints       = critical,CA:FALSE
keyUsage               = critical,digitalSignature
extendedKeyUsage       = clientAuth
subjectKeyIdentifier   = hash
authorityKeyIdentifier = keyid,issuer
EXTEOF
        ) 2>/dev/null
      ;;
  esac

  # Nettoyage CSR
  rm -f "${name}.csr"
  echo "  ✓ ${name}.crt ($(openssl x509 -noout -fingerprint -sha256 -in "${name}.crt" 2>/dev/null | cut -d= -f2))"
}

# ── Génération ────────────────────────────────────────────────────────────────
gen_cert "ca"     "$CA_CN"          "ca"
gen_cert "broker" "nidan-broker"    "server"
gen_cert "server" "nidan-server"    "server"
gen_cert "client" "nidan-client"    "client"

# ── Permissions ───────────────────────────────────────────────────────────────
chmod 644 ./*.crt
chmod 600 ./*.key

# ── Vérification ──────────────────────────────────────────────────────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Vérification des chaînes de confiance"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

for cert in broker server client; do
  result=$(openssl verify -CAfile ca.crt "${cert}.crt" 2>&1)
  if echo "$result" | grep -q "OK"; then
    echo "  ✓ ${cert}.crt → vérifié par ca.crt"
  else
    echo "  ✗ ${cert}.crt → ÉCHEC: $result"
    exit 1
  fi
done

# ── Résumé ────────────────────────────────────────────────────────────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  PKI NIDAN générée dans : $OUT_DIR/"
echo ""
echo "  Fichiers :"
ls -1 "$OUT_DIR"/*.{crt,key} 2>/dev/null | while read f; do
  printf "    %-25s  %s\n" "$(basename "$f")" "$(du -sh "$f" 2>/dev/null | cut -f1)"
done
echo ""
echo "  À faire :"
echo "    1. Copier les certs dans /etc/nidan/certs/"
echo "    2. Référencer les chemins dans les fichiers .toml"
echo "    3. Protéger les .key (chmod 600, propriétaire nidan)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
