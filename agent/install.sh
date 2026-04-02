#!/bin/bash
# Ozma Agent — One-line installer
#
# Usage:
#   curl -fsSL https://ozma.dev/install-agent | bash
#   curl -fsSL https://ozma.dev/install-agent | bash -s -- --controller https://ozma.hrdwrbob.net
#   curl -fsSL https://ozma.dev/install-agent | bash -s -- --controller https://ozma.hrdwrbob.net --name my-pc
#
# What it does:
#   1. Checks for Python 3.11+
#   2. uv pip installs ozma-agent
#   3. Runs ozma-agent install (registers as background service)
#   4. The machine appears in your dashboard
#
# Works on: Linux (systemd), macOS (launchd)
# For Windows: download the .exe installer from ozma.dev/download

set -e

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BOLD}Ozma Agent Installer${NC}"
echo ""

# Parse args
CONTROLLER=""
NAME=$(hostname)
for arg in "$@"; do
    case $arg in
        --controller=*) CONTROLLER="${arg#*=}" ;;
        --controller) shift_next=controller ;;
        --name=*) NAME="${arg#*=}" ;;
        --name) shift_next=name ;;
        *)
            if [ "$shift_next" = "controller" ]; then
                CONTROLLER="$arg"
                shift_next=""
            elif [ "$shift_next" = "name" ]; then
                NAME="$arg"
                shift_next=""
            fi
            ;;
    esac
done

# Check Python
PYTHON=""
for cmd in python3 python; do
    if command -v $cmd &>/dev/null; then
        ver=$($cmd -c "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')" 2>/dev/null)
        major=$(echo "$ver" | cut -d. -f1)
        minor=$(echo "$ver" | cut -d. -f2)
        if [ "$major" -ge 3 ] && [ "$minor" -ge 11 ]; then
            PYTHON=$cmd
            break
        fi
    fi
done

if [ -z "$PYTHON" ]; then
    echo -e "${RED}Python 3.11+ required but not found.${NC}"
    echo ""
    echo "Install Python:"
    if [ "$(uname)" = "Darwin" ]; then
        echo "  brew install python@3.12"
    elif command -v apt &>/dev/null; then
        echo "  sudo apt install python3.12 python3.12-venv"
    elif command -v dnf &>/dev/null; then
        echo "  sudo dnf install python3.12"
    elif command -v pacman &>/dev/null; then
        echo "  sudo pacman -S python"
    fi
    exit 1
fi

echo -e "Python: ${GREEN}$($PYTHON --version)${NC}"

# Install ozma-agent
echo ""
echo -e "${BOLD}Installing ozma-agent...${NC}"
uv pip install --quiet --upgrade ozma-agent 2>/dev/null || \
    uv pip install --quiet --upgrade --user ozma-agent || {
        echo -e "${YELLOW}uv pip install failed. Trying from source...${NC}"
        uv pip install --quiet --upgrade --user "git+https://github.com/ozmalabs/ozma.git#subdirectory=agent"
    }

# Find the installed binary
AGENT_BIN=""
for p in "$($PYTHON -m site --user-base)/bin/ozma-agent" \
         "$HOME/.local/bin/ozma-agent" \
         "$(dirname $($PYTHON -c 'import sys; print(sys.executable)'))/ozma-agent"; do
    if [ -x "$p" ]; then
        AGENT_BIN="$p"
        break
    fi
done

if [ -z "$AGENT_BIN" ]; then
    AGENT_BIN="$PYTHON -m cli"
fi

echo -e "Agent: ${GREEN}$AGENT_BIN${NC}"

# Prompt for controller URL if not provided
if [ -z "$CONTROLLER" ]; then
    echo ""
    echo -e "${BOLD}Enter your controller URL${NC} (e.g., https://ozma.hrdwrbob.net):"
    read -p "> " CONTROLLER
    if [ -z "$CONTROLLER" ]; then
        echo -e "${YELLOW}No controller URL. You can set it later:${NC}"
        echo "  ozma-agent config --set controller https://your-controller"
        echo "  ozma-agent install"
        exit 0
    fi
fi

# Install as background service
echo ""
echo -e "${BOLD}Installing as background service...${NC}"
echo "  Machine name: $NAME"
echo "  Controller:   $CONTROLLER"
echo ""

$AGENT_BIN install --name "$NAME" --controller "$CONTROLLER" && {
    echo ""
    echo -e "${GREEN}${BOLD}Done!${NC}"
    echo ""
    echo "  Your machine is now in the ozma mesh."
    echo "  Open your dashboard to see it: $CONTROLLER"
    echo ""
    echo "  Commands:"
    echo "    ozma-agent status      Check if running"
    echo "    ozma-agent logs        View logs"
    echo "    ozma-agent uninstall   Remove"
    echo ""
} || {
    echo -e "${RED}Service install failed. Try running manually:${NC}"
    echo "  ozma-agent run --name $NAME --controller $CONTROLLER"
}
