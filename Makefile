# OS Detection
OS := $(shell uname -s)
ARCH := $(shell uname -m)

# Paths
CARGO_BIN_DIR := $(HOME)/.cargo/bin

# Tool Commands
RUSTUP := $(CARGO_BIN_DIR)/rustup
CARGO := $(CARGO_BIN_DIR)/cargo
CROSS := $(CARGO_BIN_DIR)/cross
TRUNK := $(CARGO_BIN_DIR)/trunk
WASM_BINDGEN := $(CARGO_BIN_DIR)/wasm-bindgen
CARGO_SET_VERSION := $(CARGO_BIN_DIR)/cargo-set-version

# Explicitly force stable/nightly to avoid system-wide overrides
CARGO_STABLE     := $(CARGO) +stable
CARGO_NIGHTLY    := $(CARGO) +nightly
CROSS            := cross
TRUNK            := trunk

# Colors for terminal output
AQUA  := \033[36m
RESET := \033[0m
BOLD  := \033[1m

.DEFAULT_GOAL := help

.PHONY: help
help: ## Display this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make $(AQUA)<target>$(RESET)\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  $(AQUA)%-26s$(RESET) %s\n", $$1, $$2 } /^##@/ { printf "\n$(BOLD)%s$(RESET)\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

##@ Prerequisites:

.PHONY: install-tools
install-tools: rustup install-nightly-fmt cross trunk wasm-bindgen cargo-set-version ## Install required development tools

.PHONY: install-nightly-fmt
install-nightly-fmt: ## Install nightly toolchain specifically for formatting
	@echo "📦 Ensuring nightly rustfmt is available"
	@$(RUSTUP) toolchain install nightly --component rustfmt --profile minimal
	@echo "✅ Nightly rustfmt ready"

.PHONY: rustup
rustup: $(RUSTUP) ## Install Rust toolchain and cargo

$(RUSTUP):
	@echo "📦 Installing cargo"
	@curl -sL https://sh.rustup.rs | sh -s -- -y
	@echo "✅ Cargo installed"

.PHONY: cross
cross: $(CROSS) ## Install cross (multi-platform build tool)

$(CROSS):
	@echo "📦 Installing cross"
	@$(CARGO) install cross
	@echo "✅ Cross installed"

.PHONY: trunk
trunk: $(TRUNK) ## Install trunk (frontend build tool)

$(TRUNK):
	@echo "📦 Installing trunk"
	@$(CARGO) install trunk
	@echo "✅ Trunk installed"

.PHONY: wasm-bindgen
wasm-bindgen: $(WASM_BINDGEN) ## Install wasm-bindgen CLI (for frontend builds)

$(WASM_BINDGEN):
	@echo "📦 Installing wasm-bindgen CLI"
	@$(CARGO) install wasm-bindgen-cli
	@echo "✅ wasm-bindgen CLI installed"

.PHONY: cargo-set-version
cargo-set-version: $(CARGO_SET_VERSION) ## Install cargo-set-version (for version management)

$(CARGO_SET_VERSION):
	@echo "📦 Installing $@"
	@$(CARGO) install cargo-edit
	@echo "✅ $@ installed"

##@ Development:

.PHONY: test
test: ## Run all workspace tests (Stable)
	@echo "==> Running tests (stable)"
	$(CARGO_STABLE) test --workspace

.PHONY: lint
lint: ## Run clippy linter (Stable)
	@echo "==> Running clippy (stable)"
	$(CARGO_STABLE) clippy --workspace -- -D warnings

.PHONY: lint-fix
lint-fix: ## Automatically fix clippy suggestions (Stable)
	@echo "==> Applying clippy auto-fixes"
	$(CARGO_STABLE) clippy --fix --workspace --allow-dirty --allow-staged -- -D clippy::uninlined_format_args

.PHONY: fmt
fmt: ## Format all code using nightly rules (Compact)
	@echo "==> Formatting code (nightly)"
	$(CARGO_NIGHTLY) fmt --all

.PHONY: fmt-check
fmt-check: ## Check if code follows formatting rules (Nightly)
	@echo "==> Checking formatting (nightly)"
	$(CARGO_NIGHTLY) fmt --all -- --check
