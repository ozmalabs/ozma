#!/usr/bin/env bash
# controller/wireplumber/install.sh — Install Ozma WirePlumber audio routing.
#
# Copies 51-ozma.conf and ozma-routing.lua to the WirePlumber user config
# directory, then restarts WirePlumber.
#
# After installation, the Controller uses pw-metadata (1 call per switch)
# instead of pw-link (2–4 calls per switch).  WirePlumber manages link
# lifecycle, including retry when nodes reconnect.
#
# Usage:
#   bash controller/wireplumber/install.sh
#   bash controller/wireplumber/install.sh --uninstall

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WP_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/wireplumber"
CONF_D="$WP_DIR/wireplumber.conf.d"
SCRIPTS_DIR="$WP_DIR/scripts/custom"  # WP looks for Lua scripts relative to config dirs

uninstall() {
    echo "Removing Ozma WirePlumber config..."
    rm -f "$CONF_D/51-ozma.conf"
    rm -f "$SCRIPTS_DIR/ozma-routing.lua"
    echo "Done. Restarting WirePlumber..."
    systemctl --user restart wireplumber && echo "WirePlumber restarted."
    exit 0
}

[[ "${1:-}" == "--uninstall" ]] && uninstall

echo "Installing Ozma WirePlumber audio routing..."
mkdir -p "$CONF_D" "$SCRIPTS_DIR"

cp "$SCRIPT_DIR/51-ozma.conf"      "$CONF_D/51-ozma.conf"
cp "$SCRIPT_DIR/ozma-routing.lua"  "$SCRIPTS_DIR/ozma-routing.lua"

echo "  $CONF_D/51-ozma.conf"
echo "  $SCRIPTS_DIR/ozma-routing.lua"

echo ""
echo "Restarting WirePlumber..."
systemctl --user restart wireplumber

# Wait briefly for WP to start
sleep 1

# Verify the metadata namespace was created
if pw-metadata -n ozma 2>/dev/null | grep -q '.*' 2>/dev/null || \
   pw-metadata -n ozma set 0 active_node "" >/dev/null 2>&1; then
    echo "  ozma metadata namespace: OK"
else
    echo "  Warning: ozma metadata namespace not detected."
    echo "  Check: journalctl --user -u wireplumber -n 50"
fi

echo ""
echo "Done. Enable WirePlumber mode in the Controller:"
echo "  export OZMA_AUDIO_WIREPLUMBER=1"
echo "  python3 controller/main.py --virtual-only"
echo ""
echo "To uninstall:  bash controller/wireplumber/install.sh --uninstall"
