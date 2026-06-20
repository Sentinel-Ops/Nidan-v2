#!/usr/bin/env bash
# =============================================================================
# NIDAN — Test de validation du chiffrement E2E des inputs
# =============================================================================
# Vérifie que les événements clavier/souris (client → serveur) sont bien
# protégés par le chiffrement de bout en bout (ChaCha20-Poly1305), en
# reproduisant exactement le format fil utilisé en production.
#
# Contrôles effectués :
#   1. Échange de clés X25519 + dérivation HKDF (clé contrôle dédiée)
#   2. Chiffrement d'un batch clavier + souris
#   3. Confidentialité : aucune structure JSON visible en clair
#   4. Déchiffrement serveur : événements récupérés intacts
#   5. Intégrité : trame altérée ou nonce incorrect → rejetés (AEAD)
#   6. Fraîcheur : nonce unique par message (anti-rejeu)
#
# Usage :
#   ./test-inputs-e2e.sh            # lance le test
#   ./test-inputs-e2e.sh --keep     # conserve le projet de test temporaire
#
# À lancer depuis la racine du projet NIDAN (où se trouve Cargo.toml).
# =============================================================================

set -uo pipefail

G='\033[0;32m'; R='\033[0;31m'; B='\033[0;34m'; NC='\033[0m'
ok()   { echo -e "${G}✓${NC} $1"; }
ko()   { echo -e "${R}✗${NC} $1"; }
info() { echo -e "${B}▶${NC} $1"; }

KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

# ── Vérif racine projet ───────────────────────────────────────────────────────
if [ ! -f Cargo.toml ] || [ ! -d nidan-common ] || [ ! -d nidan-proto ]; then
  ko "Lance ce script depuis la racine du projet NIDAN (où est Cargo.toml)."
  exit 1
fi
command -v cargo >/dev/null 2>&1 || { ko "cargo introuvable — installe Rust."; exit 1; }

ROOT="$(pwd)"
TESTDIR="$(mktemp -d /tmp/nidan-inputs-e2e.XXXXXX)"
cleanup() { [ "$KEEP" = "0" ] && rm -rf "$TESTDIR"; }
trap cleanup EXIT

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  NIDAN — Validation du chiffrement E2E des inputs"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# ── Génération du projet de test ──────────────────────────────────────────────
info "Préparation du test (lié aux crates réelles du projet)..."
mkdir -p "$TESTDIR/src"

cat > "$TESTDIR/Cargo.toml" << EOF
[package]
name = "nidan-inputs-e2e"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "nidan-inputs-e2e"
path = "src/main.rs"

[dependencies]
nidan-common = { path = "$ROOT/nidan-common" }
nidan-proto  = { path = "$ROOT/nidan-proto" }
serde_json   = "1"
EOF

cat > "$TESTDIR/src/main.rs" << 'RUSTEOF'
use nidan_common::crypto::{KeyExchange, derive_session_keys, StreamCipher, random_bytes};
use nidan_proto::{InputBatch, InputEvent, InputEventPayload, KeyEvent, MouseEvent};

fn ok(msg: &str) { println!("  \x1b[32m\u{2713}\x1b[0m {}", msg); }
fn section(msg: &str) { println!("\n\x1b[34m\u{2501}\u{2501}\u{2501} {} \u{2501}\u{2501}\u{2501}\x1b[0m", msg); }

fn main() {
    let mut pass = 0; let mut fail = 0;
    let mut check = |cond: bool, msg: &str| {
        if cond { ok(msg); pass += 1; }
        else { println!("  \x1b[31m\u{2717}\x1b[0m {}", msg); fail += 1; }
    };

    section("1. Echange de cles (X25519 + HKDF)");
    let client_kx = KeyExchange::new();
    let server_kx = KeyExchange::new();
    let cn = random_bytes(32);
    let sn = random_bytes(32);
    let csec = client_kx.shared_secret(&server_kx.public).unwrap();
    let ssec = server_kx.shared_secret(&client_kx.public).unwrap();
    check(csec == ssec, "Secret ECDH partage identique");
    let ck = derive_session_keys(&csec, &cn, &sn).unwrap();
    let sk = derive_session_keys(&ssec, &cn, &sn).unwrap();
    check(ck.control.as_bytes() == sk.control.as_bytes(), "Cle de controle (inputs) identique des deux cotes");
    check(ck.control.as_bytes() != ck.video.as_bytes(), "Cle inputs distincte de la cle video (HKDF separe)");

    let mut client_cipher = StreamCipher::new(&ck.control);
    let server_cipher = StreamCipher::new(&sk.control);

    section("2. Chiffrement des evenements clavier/souris");
    let batch = InputBatch { events: vec![
        InputEvent { seq: 1, event_type: 1, timestamp_ms: 1000,
            event: Some(InputEventPayload::Key(KeyEvent { keycode: 65, scancode: 38, shift: false, ctrl: false, alt: false, meta: false, repeat: false })) },
        InputEvent { seq: 2, event_type: 3, timestamp_ms: 1010,
            event: Some(InputEventPayload::Mouse(MouseEvent { button: 0, x: 0.5, y: 0.5, scroll_dx: 0.0, scroll_dy: 0.0, monitor_idx: 0 })) },
        InputEvent { seq: 3, event_type: 4, timestamp_ms: 1020,
            event: Some(InputEventPayload::Mouse(MouseEvent { button: 1, x: 0.5, y: 0.5, scroll_dx: 0.0, scroll_dy: 0.0, monitor_idx: 0 })) },
    ]};
    let json = serde_json::to_vec(&batch).unwrap();
    check(!json.is_empty(), &format!("InputBatch serialise ({} octets)", json.len()));
    let (ct, nonce) = client_cipher.encrypt(&json).unwrap();
    let mut wire = vec![1u8];
    wire.extend_from_slice(&nonce);
    wire.extend_from_slice(&ct);
    check(wire[0] == 1, &format!("Trame chiffree construite ({} octets, flag=1)", wire.len()));
    check(ct != json, "Le ciphertext differe du plaintext");

    section("3. Confidentialite (aucune fuite en clair)");
    for needle in [&b"events"[..], &b"keycode"[..], &b"button"[..], &b"seq"[..]] {
        let leaked = wire.windows(needle.len()).any(|w| w == needle);
        check(!leaked, &format!("Champ \"{}\" invisible dans la trame", String::from_utf8_lossy(needle)));
    }

    section("4. Dechiffrement et injection (cote serveur)");
    check(wire[0] == 1, "Serveur : flag chiffre reconnu");
    let rn = &wire[1..13];
    let rc = &wire[13..];
    let dec = server_cipher.decrypt(rc, rn).expect("dechiffrement serveur");
    let rec: InputBatch = serde_json::from_slice(&dec).unwrap();
    check(rec.events.len() == 3, "Les 3 evenements sont recuperes");
    check(rec.events[0].event_type == 1, "Evenement 1 : touche (type 1) intact");
    check(rec.events[1].event_type == 3, "Evenement 2 : deplacement souris (type 3) intact");
    check(rec.events[2].event_type == 4, "Evenement 3 : clic (type 4) intact");
    if let Some(InputEventPayload::Key(k)) = &rec.events[0].event {
        check(k.keycode == 65, "Keycode de la touche preserve (65 = 'a')");
    } else { check(false, "Payload clavier manquant"); }

    section("5. Integrite (detection d'alteration AEAD)");
    let mut tampered = rc.to_vec();
    if !tampered.is_empty() { tampered[0] ^= 0xFF; }
    check(server_cipher.decrypt(&tampered, rn).is_err(), "Une trame alteree est rejetee (Poly1305 MAC)");
    let mut bad_nonce = [0u8; 12];
    bad_nonce[..8].copy_from_slice(&999u64.to_le_bytes());
    check(server_cipher.decrypt(rc, &bad_nonce).is_err(), "Un nonce incorrect est rejete");

    section("6. Fraicheur (nonces uniques par message)");
    let (_, na) = client_cipher.encrypt(b"batch A").unwrap();
    let (_, nb) = client_cipher.encrypt(b"batch B").unwrap();
    check(nonce != na && na != nb, "Chaque message utilise un nonce different");

    println!("\n\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    if fail == 0 {
        println!("  \x1b[32m\u{2713}\u{2713}\u{2713} CHIFFREMENT E2E DES INPUTS VALIDE\x1b[0m");
        println!("  {} controles reussis", pass);
        println!("  Clavier et souris : chiffres, confidentiels, integres,");
        println!("  proteges contre le rejeu (nonce unique par message).");
    } else {
        println!("  \x1b[31m\u{2717} ECHEC : {} controle(s) sur {} en echec\x1b[0m", fail, pass + fail);
    }
    println!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    std::process::exit(if fail == 0 { 0 } else { 1 });
}
RUSTEOF

# ── Compilation + exécution ───────────────────────────────────────────────────
info "Compilation du test..."
if ! ( cd "$TESTDIR" && cargo build --release 2>"$TESTDIR/build.log" ); then
  ko "Échec de compilation. Erreurs :"
  grep -E "^error" "$TESTDIR/build.log" | head -10
  echo ""
  echo "Astuce : si l'erreur mentionne un linker (cc/lld), lance d'abord :"
  echo "  cargo clean -p nidan-common && cargo clean -p nidan-proto"
  exit 1
fi
ok "test compilé"
echo ""

# Lancer le test (son code de sortie détermine le résultat global)
"$TESTDIR/target/release/nidan-inputs-e2e"
RESULT=$?

echo ""
[ "$KEEP" = "1" ] && info "Projet de test conservé : $TESTDIR"
exit $RESULT
