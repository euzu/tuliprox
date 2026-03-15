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
MDBOOK := $(CARGO_BIN_DIR)/mdbook

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
install-tools: rustup install-nightly-fmt cross trunk wasm-bindgen cargo-set-version mdbook markdownlint ## Install required development tools

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

.PHONY: mdbook
mdbook: $(MDBOOK) ## Install mdBook (documentation generator)

$(MDBOOK):
	@echo "📦 Installing mdBook"
	@$(CARGO) install mdbook
	@echo "✅ mdBook installed"

.PHONY: markdownlint
markdownlint: ## Install markdownlint-cli2 (requires npm)
	@echo "📦 Installing markdownlint-cli2"
	@command -v npm >/dev/null 2>&1 || { \
		echo "❌ npm not found. Please install Node.js and npm first."; \
		exit 1; \
	}
	@npm install -g markdownlint-cli2
	@echo "✅ markdownlint-cli2 installed"

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

.PHONY: markdown-lint
markdown-lint: ## Lint markdown files
	@echo "==> Linting markdown files"
	@command -v markdownlint-cli2 >/dev/null 2>&1 || { \
		echo "❌ markdownlint-cli2 not found. Install with: npm install -g markdownlint-cli2"; \
		exit 1; \
	}
	@markdownlint-cli2 "**/*.md"
	@echo "✅ Markdown linting complete"

.PHONY: docs
docs: mdbook ## Build static documentation into frontend/build/docs
	@echo "==> Building documentation"
	@$(MDBOOK) build
	@echo "✅ Documentation built at frontend/build/docs"

.PHONY: docs-serve
docs-serve: mdbook ## Serve documentation locally with mdBook
	@echo "==> Serving documentation"
	@$(MDBOOK) serve --open

.PHONY: docs-clean
docs-clean: ## Remove generated documentation output
	@echo "==> Removing generated documentation"
	@rm -rf frontend/build/docs
	@echo "✅ Documentation output removed"

.PHONY: web-dist
web-dist: mdbook trunk ## Build static documentation and frontend assets
	@echo "==> Building web assets"
	@./bin/build_fe.sh release
	@echo "✅ Web assets built"
