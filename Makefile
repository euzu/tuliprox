# OS Detection
OS := $(shell uname -s)
ARCH := $(shell uname -m)

# Paths
PROJECT_DIR ?= $(patsubst %/,%,$(dir $(abspath $(lastword $(MAKEFILE_LIST)))))
CARGO_HOME ?= $(HOME)/.cargo
CARGO_BIN_DIR ?= $(CARGO_HOME)/bin

# Tool Commands
resolve_tool = $(or $(shell command -v $(1) 2>/dev/null),$(CARGO_BIN_DIR)/$(1))
RUSTUP ?= $(call resolve_tool,rustup)
CARGO ?= $(call resolve_tool,cargo)
CROSS ?= $(call resolve_tool,cross)
TRUNK ?= $(call resolve_tool,trunk)
WASM_BINDGEN ?= $(call resolve_tool,wasm-bindgen)
CARGO_SET_VERSION ?= $(call resolve_tool,cargo-set-version)
CARGO_MACHETE ?= $(call resolve_tool,cargo-machete)
MDBOOK ?= $(call resolve_tool,mdbook)
PINNED_NIGHTLY_TOOLCHAIN := nightly-2026-09-21
NIGHTLY_TOOLCHAIN ?= $(shell \
	if $(RUSTUP) toolchain list 2>/dev/null | grep -q '^$(PINNED_NIGHTLY_TOOLCHAIN)'; then \
		echo $(PINNED_NIGHTLY_TOOLCHAIN); \
	else \
		INSTALLED_NIGHTLY=$$($(RUSTUP) toolchain list 2>/dev/null | grep '^nightly' | head -n1 | cut -d' ' -f1 | sed -E 's/-(x86_64|aarch64|arm|i686|riscv64).*//'); \
		if [ -n "$$INSTALLED_NIGHTLY" ]; then echo "$$INSTALLED_NIGHTLY"; else echo nightly; fi; \
	fi)

# Explicitly force stable/nightly to avoid system-wide overrides
CARGO_STABLE     := $(CARGO) +stable
CARGO_NIGHTLY    := $(CARGO) +$(NIGHTLY_TOOLCHAIN)

# Colors for terminal output
AQUA  := \033[36m
RESET := \033[0m
BOLD  := \033[1m

.DEFAULT_GOAL := help

# Number of CPUs (portable): try GNU `nproc`, then POSIX `getconf`, then macOS `sysctl`
CPU_COUNT := $(shell nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 1)
CARGO_BUILD_JOBS := $(CPU_COUNT)

# Support positional argument for serve: make serve <settings_folder>
ifeq ($(firstword $(MAKECMDGOALS)),serve)
  SETTINGS_ARG := $(word 2,$(MAKECMDGOALS))
  ifneq ($(SETTINGS_ARG),)
    SETTINGS_FOLDER ?= $(SETTINGS_ARG)
    EXTRA_GOALS := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
    $(eval .PHONY: $(EXTRA_GOALS))
    $(eval $(EXTRA_GOALS):;@:)
  endif
endif
SETTINGS_FOLDER ?= $(settings_folder)
ifeq ($(SETTINGS_FOLDER),)
  SETTINGS_FOLDER := $(TULIPROX_HOME)
endif

.PHONY: help
help: ## Display this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make $(AQUA)<target>$(RESET)\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  $(AQUA)%-26s$(RESET) %s\n", $$1, $$2 } /^##@/ { printf "\n$(BOLD)%s$(RESET)\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

##@ Prerequisites:

.PHONY: install-tools
install-tools: rustup install-nightly-fmt cross trunk wasm-bindgen cargo-set-version cargo-machete mdbook markdownlint ## Install required development tools

.PHONY: install-nightly-fmt
install-nightly-fmt: ## Install nightly toolchain specifically for formatting and clippy
	@echo "📦 Ensuring nightly rustfmt and clippy are available"
	@$(RUSTUP) toolchain install $(NIGHTLY_TOOLCHAIN) --component rustfmt --component clippy --profile minimal
	@echo "✅ Nightly rustfmt and clippy ready"

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

.PHONY: cargo-machete
cargo-machete: $(CARGO_MACHETE) ## Install cargo-machete (unused dependency detector)

$(CARGO_MACHETE):
	@echo "📦 Installing $@"
	@$(CARGO) install cargo-machete
	@echo "✅ $@ installed"

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

.PHONY: verify validate ci
verify: ## Run format, clippy, markdown lint, tests, and trunk build (stops on error)
	@$(MAKE) fmt
	@$(MAKE) lint
	@$(MAKE) markdown-lint
	@$(MAKE) test
	@$(MAKE) trunk-build
	@$(MAKE) cargo-machete-check

validate: verify ## Alias for verify
ci: verify ## Alias for verify

.PHONY: trunk-build
trunk-build: ## Build frontend with Trunk (WASM)
	@echo "==> Building frontend (trunk)"
	@cd frontend && NO_COLOR=true $(TRUNK) build

.PHONY: cargo-machete-check
cargo-machete-check: ## Detect unused dependencies across all crates (auto-installs cargo-machete if missing)
	@echo "==> Detecting unused dependencies"
	@command -v $(CARGO_MACHETE) >/dev/null 2>&1 || { \
		echo "📦 cargo-machete not found, installing..."; \
		$(CARGO) install cargo-machete; \
		echo "✅ cargo-machete installed"; \
	}
	@$(CARGO_MACHETE)

.PHONY: test
test: ## Run all workspace tests (Stable) — use detected CPU count for parallelism
	@echo "==> Running tests (stable) with $(CPU_COUNT) jobs/threads"
	@TMPDIR="$${TMPDIR:-/tmp}" RUST_TEST_THREADS=$(CPU_COUNT) ./bin/test.sh -j$(CPU_COUNT) --workspace -- --test-threads=$(CPU_COUNT)

.PHONY: admission-test
admission-test: ## Verify admission, provider-slot, seek/reopen, shared-stream, and cleanup invariants
	@echo "==> Running HTTP admission and provider lifecycle tests"
	@$(CARGO_STABLE) test --package tuliprox api_utils::tests:: -- --test-threads=1
	@echo "==> Running client stream lifecycle tests"
	@$(CARGO_STABLE) test --package tuliprox api::model::streams::active_client_stream::tests:: -- --test-threads=1
	@echo "==> Running session, eviction, and provider-slot lifecycle tests"
	@$(CARGO_STABLE) test --package tuliprox-session -- --test-threads=1

.PHONY: testkit-test
testkit-test: ## Run unit tests in tuliprox-testkit
	@echo "==> Running testkit unit tests"
	@$(CARGO_STABLE) test --package tuliprox-testkit

.PHONY: testkit-e2e
testkit-e2e: ## Run testkit E2E scenario suite against freshly built SUT
	@echo "==> Building tuliprox and tuliprox-testkit"
	@$(CARGO_STABLE) build --package tuliprox --package tuliprox-testkit
	@echo "==> Executing testkit scenario suite"
	@./bin/run-testkit-scenarios.sh

.PHONY: testkit-scenario
testkit-scenario: ## Run one testkit scenario: make testkit-scenario SCENARIO=<name> [RUN_ID=<id>]
	@if [ -z "$(SCENARIO)" ]; then \
		echo "❌ Error: SCENARIO is required."; \
		echo "Usage: make testkit-scenario SCENARIO=<scenario-name>"; \
		echo "Available scenarios:"; \
		ls test/fixtures/testkit/scenarios/*.yml | xargs -n1 basename | sed 's/\.yml$$//' | sed 's/^/  - /'; \
		exit 1; \
	fi
	@if [ ! -f "test/fixtures/testkit/scenarios/$(SCENARIO).yml" ]; then \
		echo "❌ Unknown scenario: $(SCENARIO)"; \
		exit 1; \
	fi
	@echo "==> Building tuliprox and tuliprox-testkit"
	@$(CARGO_STABLE) build --package tuliprox --package tuliprox-testkit
	@mkdir -p testkit-report/$(SCENARIO)
	@echo "==> Running scenario $(SCENARIO)"
	@TULIPROX_TESTKIT_SUT_BINARY="$(PROJECT_DIR)/target/debug/tuliprox" \
		./target/debug/tuliprox-testkit controller \
		--scenario test/fixtures/testkit/scenarios/$(SCENARIO).yml \
		--report-directory testkit-report/$(SCENARIO) \
		$(if $(RUN_ID),--run-id $(RUN_ID),)
	@echo "==> Report: testkit-report/$(SCENARIO)/summary.txt"

.PHONY: build
build: ## Build the entire workspace in parallel using detected CPU count
	@echo "==> Building workspace with $(CARGO_BUILD_JOBS) jobs"
	@TMPDIR="$${TMPDIR:-/tmp}" $(CARGO_STABLE) build -j$(CARGO_BUILD_JOBS) --workspace

.PHONY: serve
serve: ## Run tuliprox server with settings folder: make serve <settings_folder>
	@if [ -z "$(SETTINGS_FOLDER)" ]; then \
		echo "❌ Error: Settings folder is required."; \
		echo "Usage: make serve <settings_folder>  OR  make serve SETTINGS_FOLDER=<settings_folder>"; \
		exit 1; \
	fi
	@echo "==> Starting tuliprox server with TULIPROX_HOME=$(SETTINGS_FOLDER)"
	TULIPROX_HOME="$(SETTINGS_FOLDER)" $(CARGO) run --release --manifest-path $(PROJECT_DIR)/Cargo.toml --package tuliprox --bin tuliprox -- -s

.PHONY: architecture-check
architecture-check: ## Verify workspace dependency direction
	@echo "==> Checking the architecture gate itself"
	./bin/check-workspace-deps-test.sh
	@echo "==> Checking workspace dependency direction"
	./bin/check-workspace-deps.sh

.PHONY: lint
lint: ## Run clippy linter (Nightly)
	@echo "==> Running clippy (nightly)"
	$(CARGO_NIGHTLY) clippy --workspace -- -D warnings

.PHONY: lint-fix
lint-fix: ## Automatically fix clippy suggestions (Nightly)
	@echo "==> Applying clippy auto-fixes"
	$(CARGO_NIGHTLY) clippy --fix --workspace --allow-dirty --allow-staged -- -D clippy::uninlined_format_args

.PHONY: fmt
fmt: ## Format all code using nightly rules (Compact)
	@echo "==> Formatting code (nightly)"
	$(CARGO_NIGHTLY) fmt --all

.PHONY: fmt-check
fmt-check: ## Check if code follows formatting rules (Nightly)
	@echo "==> Checking formatting (nightly)"
	$(CARGO_NIGHTLY) fmt --all -- --check
	@echo "==> Checking git diff for whitespace errors"
	git diff --check

.PHONY: markdown-lint mdlint
markdown-lint: ## Lint markdown files
	@echo "==> Linting markdown files"
# 	@command -v markdownlint-cli2 >/dev/null 2>&1 || { \
# 		echo "❌ markdownlint-cli2 not found. Install with: npm install -g markdownlint-cli2"; \
# 		exit 1; \
# 	}
	@npx markdownlint-cli2 "docs/src/**/*.md" "README.md" "CHANGELOG.md" "CONTRIBUTING.md" "docker/README.md"
	@echo "✅ Markdown linting complete"

mdlint: markdown-lint ## Alias for markdown-lint

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
