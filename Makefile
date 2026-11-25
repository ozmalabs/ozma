# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Rust-first Phase 4 defaults. The Python node/softnode/agent trees remain the
# one-release rollback and are reachable through the explicit legacy targets.
#
#   make            # show targets
#   make build      # release binaries: ozma-node, ozma-virtual-node, ozma-multiseat
#   make test       # cargo test --workspace
#   make install-node
#   make demo       # Rust backend (default)
#   make demo-python  # legacy Python backend, one release

CARGO ?= cargo
PREFIX ?= $(HOME)/.local
TARGET_DIR ?= target/release
RELEASE_BINS := ozma-node ozma-softnode ozma-multiseat

.DEFAULT_GOAL := help
.PHONY: help build test check fmt clippy install-node install-node-system \
        demo demo-python legacy-python ui

help:
	@echo "Ozma Rust-first developer targets"
	@echo ""
	@echo "  make build              release Rust node binaries (default runtime)"
	@echo "  make test               cargo test --workspace"
	@echo "  make check              cargo check --workspace --all-targets"
	@echo "  make fmt                cargo fmt --all -- --check (advisory today)"
	@echo "  make clippy             cargo clippy --workspace --all-targets"
	@echo "  make install-node       install Rust binaries into \$$PREFIX (default $(PREFIX))"
	@echo "  make install-node-system  system layout; run as root, no sudo inside"
	@echo "  make demo               demo with the Rust node backend (default)"
	@echo "  make demo-python        legacy Python softnode backend (one release)"
	@echo "  make legacy-python      print the deprecated Python rollback commands"
	@echo "  make ui                 build the controller web UI"

build:
	$(CARGO) build --release --bins $(addprefix -p ,$(RELEASE_BINS))

test:
	$(CARGO) test --workspace

check:
	$(CARGO) check --workspace --all-targets

fmt:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

install-node: build
	scripts/install-rust.sh --from-target $(TARGET_DIR) --prefix $(PREFIX) \
		--bin-dir $(PREFIX)/bin

install-node-system:
	scripts/install-rust.sh --system --from-target $(TARGET_DIR)

demo:
	bash demo/run_demo.sh

demo-python:
	bash demo/run_demo.sh --python

legacy-python:
	@echo "Deprecated one-release Python rollback (default is Rust):"
	@echo "  uv pip install ozma-softnode && ozma-softnode --name my-desktop"
	@echo "  bash demo/run_demo.sh --python"
	@echo "  bash install.sh --legacy-python"

ui:
	cd controller/ui && npm run build
