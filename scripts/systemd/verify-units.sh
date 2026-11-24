#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Render the unit templates with their documented default paths and run
# `systemd-analyze verify`. This checks syntax without installing anything and
# without requiring the ozma-node binary or user to exist.
#
# Usage: scripts/systemd/verify-units.sh
# Exit status: 0 pass; 1 a unit failed verification; 2 usage error.
# A missing systemd-analyze is reported as SKIP (exit 0) so the check is useful
# on hosts and in minimal containers.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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
for exe in ozma-node ozma-virtual-node; do
    printf '#!/bin/sh\nexit 0\n' >"$TMPDIR/bin/$exe"
    chmod +x "$TMPDIR/bin/$exe"
done

render() {
    src=$1
    dest=$2
    sed \
        -e "s|@BIN_DIR@|$TMPDIR/bin|g" \
        -e 's|@ENV_DIR@|/etc/ozma|g' \
        -e 's|@STATE_DIR@|/var/lib/ozma-node|g' \
        -e 's|@RUNTIME_DIR@|/run/ozma-node|g' \
        -e 's|@SUPERVISOR_STATE_DIR@|/var/lib/ozma|g' \
        -e 's|@SUPERVISOR_RUNTIME_DIR@|/run/ozma|g' \
        "$src" >"$dest"
}

failed=0
for unit in ozma-node@.service ozma-node-vm@.service ozma-virtual-node.service; do
    dest="$TMPDIR/$unit"
    render "$SCRIPT_DIR/$unit" "$dest"
    if systemd-analyze verify "$dest" 2>"$TMPDIR/$unit.err"; then
        printf 'PASS  %s\n' "$unit"
    else
        printf 'FAIL  %s\n' "$unit" >&2
        cat "$TMPDIR/$unit.err" >&2
        failed=1
    fi
done

if [ "$failed" -ne 0 ]; then
    exit 1
fi
exit 0
