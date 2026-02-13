# OS Detection
OS := $(shell uname -s)
ARCH := $(shell uname -m)

# Paths
CARGO_BIN_DIR := $(HOME)/.cargo/bin

# Tool Commands
CARGO := $(CARGO_BIN_DIR)/cargo
CROSS := $(CARGO_BIN_DIR)/cross
TRUNK := $(CARGO_BIN_DIR)/trunk


.DEFAULT_GOAL := help

# Colors for help output
AQUA := \033[36m
RESET := \033[0m
BOLD := \033[1m

.PHONY: help
help: ## Display this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make $(AQUA)<target>$(RESET)\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  $(AQUA)%-26s$(RESET) %s\n", $$1, $$2 } /^##@/ { printf "\n$(BOLD)%s$(RESET)\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

##@ Prerequisites:

.PHONY: install-tools
install-tools: 
install-tools: cargo cross trunk wasm-bindgen cargo-set-version ## Install required development tools

.PHONY: cargo
cargo: $(CARGO) ## Install Rust toolchain and cargo

$(CARGO):
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
