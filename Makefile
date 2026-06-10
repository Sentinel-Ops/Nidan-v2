# =============================================================================
# NIDAN — Makefile
# =============================================================================
# Usage :
#   make build        — compile tous les binaires (mode stub, sans dépendances système)
#   make build-full   — compile avec FFmpeg + SDL2 + xcb (Linux avec libs installées)
#   make test         — lance tous les tests unitaires
#   make pki          — génère les certificats de développement
#   make run-dev      — lance broker + server + client en mode dev local
#   make docker       — build les images Docker
#   make clean        — nettoie les artefacts de build
#   make check        — clippy + fmt check

.PHONY: all build build-full test pki run-dev run-broker run-server run-client \
        docker docker-broker docker-server docker-client \
        clean check fmt install-deps help

CARGO       := cargo
CERTS_DIR   := ./certs
CONFIG_DIR  := ./config
TARGET_DIR  := ./target/release

# ── Build ─────────────────────────────────────────────────────────────────────

all: build

## build : Compile tous les binaires en mode stub (aucune dépendance système)
build:
	@echo "▶ Build NIDAN (mode stub)..."
	$(CARGO) build --workspace
	@echo "✓ Build terminé"

## build-release : Build optimisé pour la production
build-release:
	@echo "▶ Build release NIDAN..."
	$(CARGO) build --workspace --release
	@echo "✓ Release build terminé → $(TARGET_DIR)/"

## build-full : Build avec toutes les dépendances (FFmpeg, SDL2, xcb)
build-full:
	@echo "▶ Build NIDAN (mode full — FFmpeg + SDL2 + xcb requis)..."
	$(CARGO) build --workspace \
		--features nidan-server/full \
		--features nidan-client/full
	@echo "✓ Build full terminé"

# ── Tests ─────────────────────────────────────────────────────────────────────

## test : Lance tous les tests unitaires
test:
	@echo "▶ Tests unitaires..."
	$(CARGO) test --workspace -- --test-threads=4
	@echo "✓ Tests terminés"

## test-verbose : Tests avec output détaillé
test-verbose:
	$(CARGO) test --workspace -- --nocapture --test-threads=1

# ── Qualité ───────────────────────────────────────────────────────────────────

## check : Clippy + fmt check
check:
	@echo "▶ Clippy..."
	$(CARGO) clippy --workspace -- -D warnings
	@echo "▶ Format check..."
	$(CARGO) fmt --all -- --check
	@echo "✓ Qualité OK"

## fmt : Auto-format du code
fmt:
	$(CARGO) fmt --all

# ── PKI ───────────────────────────────────────────────────────────────────────

## pki : Génère les certificats de développement
pki:
	@echo "▶ Génération PKI NIDAN..."
	@mkdir -p $(CERTS_DIR)
	@bash scripts/pki-init.sh --out-dir $(CERTS_DIR) --days 365
	@echo "✓ Certificats dans $(CERTS_DIR)/"

## pki-clean : Supprime les certificats
pki-clean:
	@echo "⚠ Suppression des certificats..."
	rm -rf $(CERTS_DIR)

# ── Exécution dev ─────────────────────────────────────────────────────────────

## run-broker : Lance le broker (requiert PKI + config)
run-broker: build
	@echo "▶ Démarrage nidan-broker..."
	NIDAN_LOG=debug \
	NIDAN_SERVER_CONFIG=$(CONFIG_DIR)/nidan-broker.toml \
	$(CARGO) run -p nidan-broker -- --config $(CONFIG_DIR)/nidan-broker.toml

## run-server : Lance le serveur (requiert DISPLAY + PKI)
run-server: build
	@echo "▶ Démarrage nidan-server..."
	NIDAN_LOG=debug \
	NIDAN_DISPLAY=100 \
	$(CARGO) run -p nidan-server -- --config $(CONFIG_DIR)/nidan-server.toml

## run-client : Lance le client
run-client: build
	@echo "▶ Démarrage nidan-client..."
	NIDAN_LOG=debug \
	$(CARGO) run -p nidan-client -- \
		--config $(CONFIG_DIR)/nidan-client.toml \
		--direct localhost:7444

## run-audit : Lance le daemon d'audit
run-audit: build
	@echo "▶ Démarrage nidan-audit..."
	NIDAN_LOG=debug \
	$(CARGO) run -p nidan-audit -- --config $(CONFIG_DIR)/nidan-audit.toml

## run-dev : Lance broker + server + client en parallèle (dev)
run-dev: build pki
	@echo "▶ Démarrage stack NIDAN dev..."
	@trap 'kill 0' EXIT; \
	NIDAN_LOG=info $(CARGO) run -p nidan-broker -- \
		--config $(CONFIG_DIR)/nidan-broker.toml &\
	sleep 1; \
	NIDAN_LOG=info $(CARGO) run -p nidan-server -- \
		--config $(CONFIG_DIR)/nidan-server.toml &\
	sleep 1; \
	NIDAN_LOG=info $(CARGO) run -p nidan-client -- \
		--config $(CONFIG_DIR)/nidan-client.toml --direct localhost:7444; \
	wait

# ── Docker ────────────────────────────────────────────────────────────────────

## docker : Build toutes les images Docker
docker: docker-broker docker-server docker-client docker-audit

docker-broker:
	docker build -f docker/Dockerfile.broker -t nidan-broker:latest .

docker-server:
	docker build -f docker/Dockerfile.server -t nidan-server:latest .

docker-client:
	docker build -f docker/Dockerfile.client -t nidan-client:latest .

docker-audit:
	docker build -f docker/Dockerfile.audit -t nidan-audit:latest .

## docker-compose-up : Lance la stack complète via docker-compose
docker-compose-up:
	docker-compose -f docker/docker-compose.yml up

# ── Utilitaires ───────────────────────────────────────────────────────────────

## install-deps : Installe les dépendances système (Debian/Ubuntu)
install-deps:
	@echo "▶ Installation des dépendances système..."
	sudo apt-get update
	sudo apt-get install -y \
		protobuf-compiler \
		libssl-dev \
		pkg-config \
		libx11-dev \
		libxcb1-dev \
		libxcb-damage0-dev \
		libxcb-shm0-dev \
		libxcb-randr0-dev \
		libxfixes-dev \
		libavcodec-dev \
		libavformat-dev \
		libavutil-dev \
		libswscale-dev \
		libsdl2-dev \
		libsdl2-ttf-dev
	@echo "✓ Dépendances installées"

## install-deps-minimal : Dépendances minimales (build mode stub)
install-deps-minimal:
	sudo apt-get update
	sudo apt-get install -y \
		protobuf-compiler \
		libssl-dev \
		pkg-config

## clean : Nettoie les artefacts de build
clean:
	$(CARGO) clean
	@echo "✓ Nettoyage terminé"

## verify-seals : Vérifie l'intégrité des enregistrements de session
verify-seals:
	@echo "▶ Vérification des sceaux de session..."
	@for f in /var/lib/nidan/sessions/*.mkv; do \
		[ -f "$$f" ] || continue; \
		seal="$${f%.mkv}.seal"; \
		[ -f "$$seal" ] || { echo "⚠ Sceau manquant: $$f"; continue; }; \
		echo "  ✓ $$f"; \
	done

## help : Affiche l'aide
help:
	@echo ""
	@echo "NIDAN — Commandes disponibles :"
	@echo ""
	@grep -E '^## [a-zA-Z_-]+ :' $(MAKEFILE_LIST) | \
		sed 's/## /  make /' | \
		sed 's/ :/:/' | \
		column -t -s':'
	@echo ""
