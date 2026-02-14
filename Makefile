# --- Setup and OS Detection ---
OS   := $(shell uname -s)
ARCH := $(shell uname -m)

# Tool Commands
CARGO            := cargo
RUSTUP           := rustup
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
install-tools: check-rustup install-nightly-fmt ## Install all required development tools
	@echo "📦 Checking and installing CLI tools..."
	@command -v cross >/dev/null || $(CARGO_STABLE) install cross
	@command -v trunk >/dev/null || $(CARGO_STABLE) install trunk
	@command -v wasm-bindgen >/dev/null || $(CARGO_STABLE) install wasm-bindgen-cli
	@command -v cargo-set-version >/dev/null || $(CARGO_STABLE) install cargo-edit
	@echo "✅ All tools installed"

.PHONY: check-rustup
check-rustup: ## Ensure rustup is installed
	@command -v $(RUSTUP) >/dev/null || (echo "❌ rustup not found. Please install it from https://rustup.rs"; exit 1)

.PHONY: install-nightly-fmt
install-nightly-fmt: ## Install nightly toolchain specifically for formatting
	@echo "📦 Ensuring nightly rustfmt is available"
	@$(RUSTUP) toolchain install nightly --component rustfmt --profile minimal
	@echo "✅ Nightly rustfmt ready"

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
