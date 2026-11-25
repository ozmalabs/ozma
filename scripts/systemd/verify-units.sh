#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Render the unit templates as system units and as least-privilege user units,
# then run `systemd-analyze verify` on both. This checks syntax without
# installing anything and without requiring the ozma-node binary or user to
# exist.
#
# Usage: scripts/systemd/verify-units.sh [--system-only|--user-only]
# Exit status: 0 pass; 1 a unit failed verification; 2 usage error.
# A missing systemd-analyze is reported as SKIP (exit 0) so the check is useful
# on hosts and in minimal containers.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

MODE=both
case "${1:-}" in
    "") ;;
    --system-only) MODE=system ;;
    --user-only) MODE=user ;;
    -h|--help)
        sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "unknown argument: $1" >&2
        exit 2
        ;;
esac

if ! command -v systemd-analyze >/dev/null 2>&1; then
    echo "SKIP  systemd-analyze is not installed; unit files were not verified"
    exit 0
fi

TMPDIR=$(mktemp -d "${TMPDIR:-/tmp}/ozma-unit-verify.XXXXXX")
trap 'rm -rf "$TMPDIR"' EXIT

# systemd-analyze insists ExecStart exists and is executable. Use harmless
# stand-ins so syntax verification needs neither an installed binary nor the
# ozma-node service user.
mkdir -p "$TMPDIR/bin"
for exe in ozma-node ozma-virtual-node ozma-multiseat ozma-meeting-detect; do
    printf '#!/bin/sh\nexit 0\n' >"$TMPDIR/bin/$exe"
    chmod +x "$TMPDIR/bin/$exe"
done

render() {
    local src=$1 dest=$2
    sed \
        -e "s|@BIN_DIR@|$TMPDIR/bin|g" \
        -e 's|@ENV_DIR@|/etc/ozma|g' \
        -e 's|@STATE_DIR@|/var/lib/ozma-node|g' \
        -e 's|@RUNTIME_DIR@|/run/ozma-node|g' \
        -e 's|@SUPERVISOR_STATE_DIR@|/var/lib/ozma|g' \
        -e 's|@SUPERVISOR_RUNTIME_DIR@|/run/ozma|g' \
        -e 's|@OZMA_USER@|ozma-node|g' \
        -e 's|@OZMA_GROUP@|ozma-node|g' \
        "$src" >"$dest"
}

# A user unit is the same template with system-only directives removed and
# WantedBy retargeted, matching scripts/install-rust.sh's user layout.
render_user() {
    local src=$1 dest=$2
    render "$src" "$dest.system"
    grep -Ev \
        '^(User|Group|SupplementaryGroups|AmbientCapabilities|CapabilityBoundingSet|DeviceAllow|ProtectHome|ProtectSystem|PrivateDevices|ReadWritePaths)=' \
        "$dest.system" |
        sed 's/^WantedBy=multi-user.target$/WantedBy=default.target/' >"$dest"
    rm -f "$dest.system"
}

failed=0
units="ozma-node@.service ozma-node-vm@.service ozma-virtual-node.service ozma-multiseat.service"

verify_one() {
    local label=$1 unit=$2 file=$3
    shift 3
    if "$@" verify "$file" 2>"$TMPDIR/$label-$unit.err"; then
        printf 'PASS  %s %s\n' "$label" "$unit"
    else
        printf 'FAIL  %s %s\n' "$label" "$unit" >&2
        cat "$TMPDIR/$label-$unit.err" >&2
        failed=1
    fi
}

if [ "$MODE" != "user" ]; then
    mkdir -p "$TMPDIR/system"
    for unit in $units; do
        dest="$TMPDIR/system/$unit"
        render "$SCRIPT_DIR/$unit" "$dest"
        # shellcheck disable=SC2086
        verify_one system "$unit" "$dest" systemd-analyze
    done
fi

if [ "$MODE" != "system" ]; then
    mkdir -p "$TMPDIR/user"
    for unit in $units; do
        dest="$TMPDIR/user/$unit"
        render_user "$SCRIPT_DIR/$unit" "$dest"
        # shellcheck disable=SC2086
        verify_one user "$unit" "$dest" systemd-analyze --user
    done
fi

if [ "$failed" -ne 0 ]; then
    exit 1
fi
exit 0
