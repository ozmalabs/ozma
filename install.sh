#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Ozma Controller — bare metal installer
#
# Installs the ozma controller on any Linux machine.
# Run as your normal user (not root). Uses sudo for system packages only.
#
# Usage:
#   curl -sSL https://ozma.dev/install.sh | bash
#   # or
#   bash install.sh
#
# What it does:
#   1. Installs system dependencies (ffmpeg, PipeWire, avahi)
#   2. Creates a Python virtual environment
#   3. Installs Python dependencies
#   4. Creates a systemd user service
#   5. Starts the controller
#
# After installation:
#   Open http://localhost:7380 in your browser
#   Install soft nodes: uv pip install ozma-softnode && ozma-softnode --name my-pc

set -euo pipefail

OZMA_DIR="${OZMA_DIR:-$HOME/.ozma}"
REPO_URL="https://github.com/ozmalabs/ozma.git"
BRANCH="main"
AGENT_REPO="ozmalabs/ozma"
AGENT_BIN_DIR="${OZMA_AGENT_BIN_DIR:-$HOME/.local/bin}"

# Colours
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BLUE}[ozma]${NC} $*"; }
ok()    { echo -e "${GREEN}[ozma]${NC} $*"; }
err()   { echo -e "${RED}[ozma]${NC} $*" >&2; }

# ── Pre-flight checks ──────────────────────────────────────────────────────

info "Ozma Controller + Agent installer"
echo ""

# Check Python 3.11+
if ! command -v python3 &>/dev/null; then
    err "Python 3 not found. Install python3 first."
    exit 1
fi

PY_VERSION=$(python3 -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')
PY_MAJOR=$(echo "$PY_VERSION" | cut -d. -f1)
PY_MINOR=$(echo "$PY_VERSION" | cut -d. -f2)
if [[ "$PY_MAJOR" -lt 3 || ("$PY_MAJOR" -eq 3 && "$PY_MINOR" -lt 11) ]]; then
    err "Python 3.11+ required (found $PY_VERSION)"
    exit 1
fi
ok "Python $PY_VERSION"

# ── System dependencies ────────────────────────────────────────────────────

info "Installing system dependencies..."

if command -v apt-get &>/dev/null; then
    # Debian/Ubuntu
    sudo apt-get update -qq
    sudo apt-get install -y -qq ffmpeg avahi-utils v4l-utils git \
        pipewire pipewire-pulse wireplumber 2>/dev/null || true
elif command -v dnf &>/dev/null; then
    # Fedora/RHEL
    sudo dnf install -y -q ffmpeg avahi-tools v4l-utils git \
        pipewire pipewire-pulseaudio wireplumber 2>/dev/null || true
elif command -v pacman &>/dev/null; then
    # Arch
    sudo pacman -S --noconfirm --needed ffmpeg avahi v4l-utils git \
        pipewire pipewire-pulse wireplumber 2>/dev/null || true
else
    info "Unknown package manager — please install manually: ffmpeg, avahi, git"
fi

ok "System dependencies installed"

# ── Clone or update repo ───────────────────────────────────────────────────

if [[ -d "$OZMA_DIR" && -d "$OZMA_DIR/.git" ]]; then
    info "Updating existing installation..."
    cd "$OZMA_DIR"
    git pull --ff-only origin "$BRANCH" 2>/dev/null || true
else
    info "Cloning ozma..."
    git clone --depth 1 --branch "$BRANCH" "$REPO_URL" "$OZMA_DIR"
    cd "$OZMA_DIR"
fi

ok "Code at $OZMA_DIR"

# ── Python virtual environment ─────────────────────────────────────────────

VENV="$OZMA_DIR/.venv"
if [[ ! -d "$VENV" ]]; then
    info "Creating virtual environment..."
    python3 -m venv "$VENV"
fi

source "$VENV/bin/activate"
uv pip install --quiet --upgrade pip
uv pip install --quiet -r controller/requirements.txt
uv pip install --quiet pynacl

ok "Python dependencies installed"

# ── Systemd user service ───────────────────────────────────────────────────

SERVICE_DIR="$HOME/.config/systemd/user"
SERVICE_FILE="$SERVICE_DIR/ozma-controller.service"

mkdir -p "$SERVICE_DIR"

cat > "$SERVICE_FILE" << EOF
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

systemctl --user daemon-reload
systemctl --user enable ozma-controller
systemctl --user start ozma-controller

ok "Systemd service installed and started"

# ── Install ozma-agent binary ──────────────────────────────────────────────

info "Installing ozma-agent..."

# Detect architecture
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64)  AGENT_ARCH="x86_64" ;;
    aarch64) AGENT_ARCH="aarch64" ;;
    arm64)   AGENT_ARCH="aarch64" ;;
    *)
        err "Unsupported architecture: $ARCH (only x86_64 and aarch64 are supported)"
        exit 1
        ;;
esac

# Fetch the latest release tag from GitHub API
AGENT_TAG=$(curl -sSfL \
    "https://api.github.com/repos/${AGENT_REPO}/releases/latest" \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['tag_name'])")

AGENT_URL="https://github.com/${AGENT_REPO}/releases/download/${AGENT_TAG}/ozma-agent-linux-${AGENT_ARCH}.AppImage"

mkdir -p "$AGENT_BIN_DIR"
curl -sSfL "$AGENT_URL" -o "$AGENT_BIN_DIR/ozma-agent"
chmod +x "$AGENT_BIN_DIR/ozma-agent"

ok "ozma-agent ${AGENT_TAG} installed to $AGENT_BIN_DIR/ozma-agent"

# Ensure bin dir is on PATH for this session
export PATH="$AGENT_BIN_DIR:$PATH"

# ── Systemd user service for ozma-agent ───────────────────────────────────

AGENT_SERVICE_FILE="$SERVICE_DIR/ozma-agent.service"

cat > "$AGENT_SERVICE_FILE" << EOF
[Unit]
Description=Ozma Agent
After=network.target ozma-controller.service
Wants=ozma-controller.service

[Service]
Type=simple
ExecStart=$AGENT_BIN_DIR/ozma-agent \
    --api-host 0.0.0.0 \
    --api-port 7381 \
    --metrics-port 9101 \
    --controller-url http://localhost:7380
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=ozma_agent=info

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable ozma-agent
systemctl --user start ozma-agent

ok "ozma-agent systemd service installed and started"

# ── Done ───────────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  Ozma Controller is running!${NC}"
echo -e "${BOLD}════════════════════════════════════════════════${NC}"
echo ""
echo -e "  ${BLUE}Dashboard:${NC}   http://localhost:7380"
echo -e "  ${BLUE}API:${NC}         http://localhost:7380/api/v1/status"
echo -e "  ${BLUE}Agent API:${NC}   http://localhost:7381/api/v1/status"
echo -e "  ${BLUE}Metrics:${NC}     http://localhost:9101/metrics"
echo -e "  ${BLUE}Logs:${NC}        journalctl --user -u ozma-controller -f"
echo -e "  ${BLUE}Agent logs:${NC}  journalctl --user -u ozma-agent -f"
echo ""
echo -e "  ${BOLD}Add machines:${NC}"
echo -e "    uv pip install ozma-softnode"
echo -e "    ozma-softnode --name my-desktop"
echo ""
echo -e "  ${BOLD}Manage:${NC}"
echo -e "    systemctl --user status ozma-controller ozma-agent"
echo -e "    systemctl --user restart ozma-controller ozma-agent"
echo -e "    systemctl --user stop ozma-controller ozma-agent"
echo ""
echo -e "  ${BOLD}Update:${NC}"
echo -e "    cd $OZMA_DIR && git pull && systemctl --user restart ozma-controller"
echo ""
echo -e "  ${GREEN}Easy things automatic. Hard things easy. Amazing things possible.${NC}"
echo ""
