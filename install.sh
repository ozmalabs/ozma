#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Ozma Controller + Rust node installer
#
# Installs the Ozma controller and, by default, the Rust `ozma-node` desktop
# node plus its systemd --user unit. The controller itself is still Python.
#
# Run as your normal user (not root). Uses sudo for system packages only.
#
# Usage:
#   curl -sSL https://ozma.dev/install.sh | bash
#   bash install.sh                     # controller + Rust node (desktop)
#   bash install.sh --dry-run           # print every action; change nothing
#   bash install.sh --skip-node         # controller only
#   bash install.sh --node-profile vm   # desktop (default), hardware, or vm
#   bash install.sh --legacy-python     # one-release Python rollback path
#   OZMA_INSTALL_LEGACY_PYTHON=1 bash install.sh
#
# What the default path does:
#   1. Installs system dependencies (ffmpeg, PipeWire, avahi, Rust build deps)
#   2. Clones/updates the repository under ~/.ozma
#   3. Creates the Python virtual environment for the controller
#   4. Builds/installs ozma-node + ozma-virtual-node and the WebRTC helper
#   5. Installs and starts the controller + ozma-node@<profile> user units
#
# The legacy path installs the deprecated ozma-agent AppImage and only prints
# the old `uv pip install ozma-softnode` guidance. It is kept for one overlap
# release; see docs/phase4-install-cutover.md.
#
# After installation:
#   Open http://localhost:7380 in your browser
#   Add machines with the Rust node:
#     ozma-node --profile desktop --name my-pc
#   On a hypervisor:
#     ozma-virtual-node --node-bin ozma-node --controller http://localhost:7380

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OZMA_DIR="${OZMA_DIR:-$HOME/.ozma}"
REPO_URL="${OZMA_REPO_URL:-https://github.com/ozmalabs/ozma.git}"
BRANCH="${OZMA_BRANCH:-main}"
AGENT_REPO="${OZMA_AGENT_REPO:-ozmalabs/ozma}"
NODE_BIN_DIR="${OZMA_NODE_BIN_DIR:-${OZMA_AGENT_BIN_DIR:-$HOME/.local/bin}}"
RUST_PREFIX="${OZMA_RUST_PREFIX:-$HOME/.local}"
SERVICE_DIR="$HOME/.config/systemd/user"
UV_BIN="${UV_BIN:-uv}"

DRY_RUN=0
LEGACY_PYTHON=0
SKIP_NODE=0
SKIP_SYSTEM_DEPS="${OZMA_SKIP_SYSTEM_DEPS:-0}"
NODE_PROFILE="${OZMA_NODE_PROFILE:-desktop}"

case "${OZMA_INSTALL_LEGACY_PYTHON:-0}" in
    1|true|yes|on) LEGACY_PYTHON=1 ;;
esac
case "${OZMA_INSTALL_DRY_RUN:-0}" in
    1|true|yes|on) DRY_RUN=1 ;;
esac

# Colours
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[0;33m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BLUE}[ozma]${NC} $*"; }
ok()    { echo -e "${GREEN}[ozma]${NC} $*"; }
warn()  { echo -e "${YELLOW}[ozma]${NC} $*" >&2; }
err()   { echo -e "${RED}[ozma]${NC} $*" >&2; }

usage() {
    sed -n '2,36p' "$0" | sed 's/^# \{0,1\}//'
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --legacy-python|--python-nodes) LEGACY_PYTHON=1 ;;
        --skip-node) SKIP_NODE=1 ;;
        --skip-system-deps) SKIP_SYSTEM_DEPS=1 ;;
        --node-profile) shift; NODE_PROFILE="${1:-}" ;;
        --node-profile=*) NODE_PROFILE="${1#*=}" ;;
        -h|--help) usage; exit 0 ;;
        *)
            err "unknown argument: $1"
            usage >&2
            exit 2
            ;;
    esac
    shift
done

case "$NODE_PROFILE" in
    desktop|hardware|vm) ;;
    *)
        err "--node-profile must be desktop, hardware, or vm (got '$NODE_PROFILE')"
        exit 2
        ;;
esac

# ---------------------------------------------------------------------------
# Dry-run aware command execution.
run() {
    if [ "$DRY_RUN" = 1 ]; then
        printf 'DRY   %s\n' "$*"
    else
        "$@"
    fi
}

run_sudo() {
    if [ "$DRY_RUN" = 1 ]; then
        printf 'DRY   sudo %s\n' "$*"
    else
        sudo "$@"
    fi
}

say() { printf '%s\n' "$*"; }

# ── Pre-flight checks ──────────────────────────────────────────────────────

info "Ozma Controller + Rust node installer"
[ "$DRY_RUN" = 1 ] && warn "dry run: no clone, install, service, or sudo command will run"
echo ""

if ! command -v python3 &>/dev/null; then
    err "Python 3 not found. The controller still requires python3."
    exit 1
fi

PY_VERSION=$(python3 -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')
PY_MAJOR=$(echo "$PY_VERSION" | cut -d. -f1)
PY_MINOR=$(echo "$PY_VERSION" | cut -d. -f2)
if [[ "$PY_MAJOR" -lt 3 || ("$PY_MAJOR" -eq 3 && "$PY_MINOR" -lt 11) ]]; then
    err "Python 3.11+ required for the controller (found $PY_VERSION)"
    exit 1
fi
ok "Python $PY_VERSION"

if ! command -v "$UV_BIN" &>/dev/null && [ "$DRY_RUN" -eq 0 ]; then
    err "uv not found. Install it (https://docs.astral.sh/uv/) or set UV_BIN."
    exit 1
fi

# ── System dependencies ────────────────────────────────────────────────────

if [ "$SKIP_SYSTEM_DEPS" = "1" ]; then
    info "Skipping system dependencies (--skip-system-deps / OZMA_SKIP_SYSTEM_DEPS=1)"
else
    info "Installing system dependencies..."
    if command -v apt-get &>/dev/null; then
        # Debian/Ubuntu. Rust build deps match scripts/clean-host/check-deps.sh.
        run_sudo apt-get update -qq
        run_sudo apt-get install -y -qq ffmpeg avahi-utils v4l-utils git curl \
            pipewire pipewire-pulse wireplumber \
            build-essential pkg-config libudev-dev libasound2-dev \
            libpipewire-0.3-dev libpam0g-dev libclang-dev clang libxdo-dev \
            cmake nasm 2>/dev/null || true
    elif command -v dnf &>/dev/null; then
        # Fedora/RHEL
        run_sudo dnf install -y -q ffmpeg avahi-tools v4l-utils git curl \
            pipewire pipewire-pulseaudio wireplumber \
            pkgconf-pkg-config gcc gcc-c++ systemd-devel alsa-lib-devel \
            pipewire-devel pam-devel clang clang-devel libxdo-devel \
            cmake nasm 2>/dev/null || true
    elif command -v pacman &>/dev/null; then
        # Arch
        run_sudo pacman -S --noconfirm --needed ffmpeg avahi v4l-utils git curl \
            pipewire pipewire-pulse wireplumber \
            pkgconf base-devel systemd-libs alsa-lib pipewire pam \
            clang xdotool cmake nasm 2>/dev/null || true
    else
        warn "Unknown package manager — install manually: ffmpeg, avahi, git, curl,"
        warn "pkg-config, clang, libudev, libasound, libpipewire, libxdo, cmake, nasm."
    fi
    ok "System dependencies installed"
fi

# ── Clone or update repo ───────────────────────────────────────────────────

CODE_DIR="$OZMA_DIR"
if [ -d "$OZMA_DIR/.git" ]; then
    info "Updating existing installation at $OZMA_DIR..."
    run git -C "$OZMA_DIR" pull --ff-only origin "$BRANCH"
    CODE_DIR="$OZMA_DIR"
elif [ -d "$SCRIPT_DIR/.git" ] && [ "$SCRIPT_DIR" != "$OZMA_DIR" ]; then
    # Running from a checkout: dry-run can inspect it without cloning.
    if [ "$DRY_RUN" = 1 ]; then
        say "DRY   git clone --depth 1 --branch $BRANCH $REPO_URL $OZMA_DIR"
        CODE_DIR="$SCRIPT_DIR"
    else
        info "Cloning ozma..."
        run git clone --depth 1 --branch "$BRANCH" "$REPO_URL" "$OZMA_DIR"
        CODE_DIR="$OZMA_DIR"
    fi
else
    info "Cloning ozma..."
    run git clone --depth 1 --branch "$BRANCH" "$REPO_URL" "$OZMA_DIR"
    CODE_DIR="$OZMA_DIR"
fi

if [ "$DRY_RUN" = 1 ] && [ "$CODE_DIR" != "$OZMA_DIR" ]; then
    ok "Code source for dry run: $CODE_DIR (would clone to $OZMA_DIR)"
else
    ok "Code at $CODE_DIR"
fi

# ── Python virtual environment (controller only) ───────────────────────────

VENV="$OZMA_DIR/.venv"
if [ ! -d "$VENV" ]; then
    info "Creating controller virtual environment..."
    run python3 -m venv "$VENV"
fi

if [ "$DRY_RUN" = 1 ]; then
    say "DRY   source $VENV/bin/activate && $UV_BIN pip install -r $CODE_DIR/controller/requirements.txt pynacl"
else
    # shellcheck disable=SC1091
    source "$VENV/bin/activate"
    "$UV_BIN" pip install --quiet --upgrade pip
    "$UV_BIN" pip install --quiet -r "$CODE_DIR/controller/requirements.txt"
    "$UV_BIN" pip install --quiet pynacl
fi

ok "Controller Python dependencies installed"

# ── Controller systemd user service ────────────────────────────────────────

CONTROLLER_SERVICE_FILE="$SERVICE_DIR/ozma-controller.service"

write_controller_unit() {
    if [ "$DRY_RUN" = 1 ]; then
        say "DRY   write $CONTROLLER_SERVICE_FILE (ozma-controller.service)"
        return 0
    fi
    mkdir -p "$SERVICE_DIR"
    cat > "$CONTROLLER_SERVICE_FILE" << EOF
[Unit]
Description=Ozma Controller
After=network.target pipewire.service
Wants=pipewire.service

[Service]
Type=simple
WorkingDirectory=$OZMA_DIR/controller
ExecStart=$VENV/bin/python main.py
Restart=on-failure
RestartSec=5
Environment=OZMA_API_HOST=0.0.0.0
Environment=OZMA_API_PORT=7380

[Install]
WantedBy=default.target
EOF
}

write_controller_unit
run systemctl --user daemon-reload
run systemctl --user enable ozma-controller
run systemctl --user start ozma-controller

ok "Controller systemd service installed"

# ── Rust node (default) ────────────────────────────────────────────────────

install_rust_node() {
    if [ -f "$SERVICE_DIR/ozma-agent.service" ]; then
        warn "Existing ozma-agent.service detected; leaving it running (deprecated)."
        warn "Disable it when ready: systemctl --user disable --now ozma-agent"
    fi

    info "Installing Rust node (profile: $NODE_PROFILE)..."
    if [ "$DRY_RUN" = 1 ]; then
        say "DRY   bash $CODE_DIR/scripts/install-rust.sh --build --prefix $RUST_PREFIX --bin-dir $NODE_BIN_DIR --units-dir $SERVICE_DIR"
        say "DRY   systemctl --user daemon-reload"
        say "DRY   systemctl --user enable --now ozma-node@$NODE_PROFILE.service"
        say "DRY   node binary: $NODE_BIN_DIR/ozma-node"
        say "DRY   webrtc helper: $RUST_PREFIX/share/ozma/webrtc/gst_webrtc_worker.py"
        ok "Rust node install planned (dry run)"
        return 0
    fi

    if [ ! -f "$CODE_DIR/scripts/install-rust.sh" ]; then
        err "scripts/install-rust.sh not found under $CODE_DIR"
        exit 1
    fi

    local args=(--prefix "$RUST_PREFIX" --bin-dir "$NODE_BIN_DIR" --units-dir "$SERVICE_DIR")
    if [[ -x "$CODE_DIR/target/release/ozma-node" && -x "$CODE_DIR/target/release/ozma-virtual-node" ]]; then
        args+=(--from-target "$CODE_DIR/target/release")
    elif command -v cargo &>/dev/null; then
        args+=(--build)
        warn "Building Rust release binaries; this can take several minutes."
    else
        err "No prebuilt ozma-node/ozma-virtual-node and cargo is not installed."
        err "Install Rust (https://rustup.rs) and re-run, or build the binaries"
        err "and run scripts/install-rust.sh --from-target <dir> directly."
        exit 1
    fi

    bash "$CODE_DIR/scripts/install-rust.sh" "${args[@]}"
    systemctl --user daemon-reload
    systemctl --user enable --now "ozma-node@${NODE_PROFILE}.service"
    export PATH="$NODE_BIN_DIR:$PATH"
    ok "Rust node installed and ozma-node@${NODE_PROFILE}.service started"
    say "  binary:  $NODE_BIN_DIR/ozma-node"
    say "  helper:  $RUST_PREFIX/share/ozma/webrtc/gst_webrtc_worker.py"
    say "  env:     $RUST_PREFIX/etc/ozma/"
}

# ── Legacy Python/AppImage path (one release) ──────────────────────────────

install_legacy_agent() {
    warn "Using the deprecated legacy Python/AppImage path (one overlap release)."
    info "Installing ozma-agent AppImage..."

    local arch agent_arch tag url
    arch="$(uname -m)"
    case "$arch" in
        x86_64)  agent_arch="x86_64" ;;
        aarch64|arm64) agent_arch="aarch64" ;;
        *)
            err "Unsupported architecture: $arch (only x86_64 and aarch64)"
            exit 1
            ;;
    esac

    if [ "$DRY_RUN" = 1 ]; then
        say "DRY   curl releases/latest from https://api.github.com/repos/$AGENT_REPO/releases/latest"
        say "DRY   curl https://github.com/$AGENT_REPO/releases/download/<tag>/ozma-agent-linux-$agent_arch.AppImage -o $NODE_BIN_DIR/ozma-agent"
        say "DRY   write $SERVICE_DIR/ozma-agent.service"
        say "DRY   systemctl --user enable --now ozma-agent"
        ok "Legacy ozma-agent install planned (dry run)"
        return 0
    fi

    tag=$(curl -sSfL \
        "https://api.github.com/repos/${AGENT_REPO}/releases/latest" \
        | python3 -c "import sys,json; print(json.load(sys.stdin)['tag_name'])")
    url="https://github.com/${AGENT_REPO}/releases/download/${tag}/ozma-agent-linux-${agent_arch}.AppImage"

    mkdir -p "$NODE_BIN_DIR"
    curl -sSfL "$url" -o "$NODE_BIN_DIR/ozma-agent"
    chmod +x "$NODE_BIN_DIR/ozma-agent"

    mkdir -p "$SERVICE_DIR"
    cat > "$SERVICE_DIR/ozma-agent.service" << EOF
[Unit]
Description=Ozma Agent (deprecated AppImage)
After=network.target ozma-controller.service
Wants=ozma-controller.service

[Service]
Type=simple
ExecStart=$NODE_BIN_DIR/ozma-agent \\
    --api-host 0.0.0.0 \\
    --api-port 7381 \\
    --metrics-port 9101 \\
    --controller-url http://localhost:7380
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=ozma_agent=info

[Install]
WantedBy=default.target
EOF

    systemctl --user daemon-reload
    systemctl --user enable --now ozma-agent
    ok "Legacy ozma-agent ${tag} installed and started"
}

if [ "$LEGACY_PYTHON" = "1" ]; then
    install_legacy_agent
elif [ "$SKIP_NODE" = "1" ]; then
    info "Skipping the node install (--skip-node)"
else
    install_rust_node
fi

# ── Done ───────────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}════════════════════════════════════════════════${NC}"
if [ "$DRY_RUN" = "1" ]; then
    echo -e "${BOLD}  Ozma install dry run complete (nothing changed)${NC}"
else
    echo -e "${BOLD}  Ozma Controller is running!${NC}"
fi
echo -e "${BOLD}════════════════════════════════════════════════${NC}"
echo ""
echo -e "  ${BLUE}Dashboard:${NC}   http://localhost:7380"
echo -e "  ${BLUE}API:${NC}         http://localhost:7380/api/v1/status"
echo -e "  ${BLUE}Logs:${NC}        journalctl --user -u ozma-controller -f"
echo ""
if [ "$LEGACY_PYTHON" = "1" ]; then
    echo -e "  ${YELLOW}${BOLD}Legacy path (deprecated, one release):${NC}"
    echo -e "    uv pip install ozma-softnode"
    echo -e "    ozma-softnode --name my-desktop"
    echo -e "    systemctl --user status ozma-controller ozma-agent"
    echo ""
    echo -e "  Roll back to the Rust node: bash install.sh"
elif [ "$SKIP_NODE" = "1" ]; then
    echo -e "  ${BOLD}Add machines (Rust):${NC}"
    echo -e "    scripts/install-rust.sh --build"
    echo -e "    ozma-node --profile desktop --name my-desktop"
    echo ""
else
    echo -e "  ${BOLD}This machine:${NC}"
    echo -e "    $NODE_BIN_DIR/ozma-node --profile $NODE_PROFILE --name $(hostname -s 2>/dev/null || echo my-pc)"
    echo -e "    systemctl --user status ozma-controller ozma-node@$NODE_PROFILE"
    echo ""
    echo -e "  ${BOLD}Add other machines (Rust):${NC}"
    echo -e "    scripts/install-rust.sh --build && ozma-node --profile desktop --name my-desktop"
    echo -e "  ${BOLD}Hypervisor:${NC}"
    echo -e "    ozma-virtual-node --node-bin $NODE_BIN_DIR/ozma-node --controller http://localhost:7380"
    echo -e "  ${BOLD}Optional WebRTC helper:${NC}"
    echo -e "    ozma-node --webrtc-helper python3 \\"
    echo -e "      --webrtc-helper-script $RUST_PREFIX/share/ozma/webrtc/gst_webrtc_worker.py"
    echo ""
fi
echo -e "  ${BOLD}Manage:${NC}"
if [ "$LEGACY_PYTHON" = "1" ]; then
    echo -e "    systemctl --user restart ozma-controller ozma-agent"
else
    echo -e "    systemctl --user restart ozma-controller ozma-node@$NODE_PROFILE"
fi
echo -e "    systemctl --user stop ozma-controller"
echo ""
echo -e "  ${BOLD}Update:${NC}"
echo -e "    cd $OZMA_DIR && git pull && systemctl --user restart ozma-controller"
echo -e "    bash $OZMA_DIR/install.sh --skip-system-deps"
echo ""
echo -e "  ${BOLD}One-release Python rollback:${NC}"
echo -e "    bash install.sh --legacy-python    # or OZMA_INSTALL_LEGACY_PYTHON=1"
echo ""
echo -e "  ${GREEN}Easy things automatic. Hard things easy. Amazing things possible.${NC}"
echo ""
