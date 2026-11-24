#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Lay out the Rust node binaries, systemd units, and environment examples.
#
# This script never uses sudo, never enables/starts a unit, never edits the
# Python softnode/node/agent trees, and never deletes anything. The Python
# implementation remains the rollback path for one release.
#
# Usage:
#   scripts/install-rust.sh --build                 # ~/.local user layout
#   scripts/install-rust.sh --from-target target/release
#   scripts/install-rust.sh --system --prefix /usr/local   # run as root
#   scripts/install-rust.sh --dry-run --build
#
# Exit status: 0 success; 1 missing binary/refused overwrite; 2 usage error.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

MODE=user
PREFIX=""
BIN_DIR=""
UNITS_DIR=""
ENV_DIR=""
STATE_DIR=""
RUNTIME_DIR=""
SUP_STATE_DIR=""
SUP_RUNTIME_DIR=""
FROM_TARGET=""
DO_BUILD=0
DRY_RUN=0
FORCE=0
INSTALL_ENV=1

usage() {
    sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
}

die() { echo "error: $*" >&2; exit 1; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --user) MODE=user ;;
        --system) MODE=system ;;
        --prefix) shift; PREFIX="${1:-}" ;;
        --prefix=*) PREFIX="${1#*=}" ;;
        --bin-dir) shift; BIN_DIR="${1:-}" ;;
        --bin-dir=*) BIN_DIR="${1#*=}" ;;
        --units-dir) shift; UNITS_DIR="${1:-}" ;;
        --units-dir=*) UNITS_DIR="${1#*=}" ;;
        --env-dir) shift; ENV_DIR="${1:-}" ;;
        --env-dir=*) ENV_DIR="${1#*=}" ;;
        --state-dir) shift; STATE_DIR="${1:-}" ;;
        --state-dir=*) STATE_DIR="${1#*=}" ;;
        --runtime-dir) shift; RUNTIME_DIR="${1:-}" ;;
        --runtime-dir=*) RUNTIME_DIR="${1#*=}" ;;
        --from-target) shift; FROM_TARGET="${1:-}" ;;
        --from-target=*) FROM_TARGET="${1#*=}" ;;
        --build) DO_BUILD=1 ;;
        --no-env) INSTALL_ENV=0 ;;
        --dry-run) DRY_RUN=1 ;;
        --force) FORCE=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
MODE_LABEL="user"
if [ "$MODE" = "system" ]; then
    MODE_LABEL="system"
    if [ "$(id -u)" -ne 0 ] && [ "$DRY_RUN" = "0" ]; then
        die "--system must be run as root (the script never invokes sudo itself)"
    fi
    PREFIX="${PREFIX:-/usr/local}"
    BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
    UNITS_DIR="${UNITS_DIR:-$PREFIX/lib/systemd/system}"
    ENV_DIR="${ENV_DIR:-/etc/ozma}"
    STATE_DIR="${STATE_DIR:-/var/lib/ozma-node}"
    RUNTIME_DIR="${RUNTIME_DIR:-/run/ozma-node}"
    SUP_STATE_DIR="${SUP_STATE_DIR:-/var/lib/ozma}"
    SUP_RUNTIME_DIR="${SUP_RUNTIME_DIR:-/run/ozma}"
else
    PREFIX="${PREFIX:-$HOME/.local}"
    BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
    UNITS_DIR="${UNITS_DIR:-$PREFIX/lib/systemd/user}"
    ENV_DIR="${ENV_DIR:-$PREFIX/etc/ozma}"
    STATE_DIR="${STATE_DIR:-$PREFIX/var/lib/ozma-node}"
    RUNTIME_DIR="${RUNTIME_DIR:-$PREFIX/var/run/ozma-node}"
    SUP_STATE_DIR="${SUP_STATE_DIR:-$PREFIX/var/lib/ozma}"
    SUP_RUNTIME_DIR="${SUP_RUNTIME_DIR:-$PREFIX/var/run/ozma}"
fi

for path in "$PREFIX" "$BIN_DIR" "$UNITS_DIR" "$ENV_DIR" "$STATE_DIR" \
            "$RUNTIME_DIR" "$SUP_STATE_DIR" "$SUP_RUNTIME_DIR"; do
    case "$path" in
        *[[:space:]\&\|]*) die "paths containing whitespace, '&', or '|' are not supported: $path" ;;
    esac
done

FROM_TARGET="${FROM_TARGET:-$REPO_ROOT/target/release}"

run() {
    if [ "$DRY_RUN" = "1" ]; then
        printf 'DRY   %s\n' "$*"
    else
        "$@"
    fi
}

say() { printf '%s\n' "$*"; }

# ---------------------------------------------------------------------------
say "Ozma Rust node install ($MODE_LABEL layout)"
say "  binaries:    $BIN_DIR"
say "  units:       $UNITS_DIR"
say "  environment: $ENV_DIR"
say "  state:       $STATE_DIR"
say "  runtime:     $RUNTIME_DIR"
say "  supervisor:  $SUP_STATE_DIR $SUP_RUNTIME_DIR"
say "  target:      $FROM_TARGET"
[ "$DRY_RUN" = "1" ] && say "  dry run:     yes (no files are written)"
say ""

if [ "$DO_BUILD" = "1" ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo is required for --build"
    say "Building release binaries (cargo build --release -p ozma-node -p ozma-softnode)"
    run cargo build --release -p ozma-node -p ozma-softnode
    FROM_TARGET="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release"
fi

NODE_SRC="$FROM_TARGET/ozma-node"
VNM_SRC="$FROM_TARGET/ozma-virtual-node"
[ -x "$NODE_SRC" ] || die "missing executable $NODE_SRC (run with --build)"
[ -x "$VNM_SRC" ] || die "missing executable $VNM_SRC (run with --build)"

install_file() {
    src=$1
    dest=$2
    mode=$3
    if [ -e "$dest" ] && ! cmp -s "$src" "$dest"; then
        if [ "$FORCE" != "1" ]; then
            die "$dest exists and differs; re-run with --force to replace it"
        fi
    fi
    run install -d -m 0755 "$(dirname "$dest")"
    run install -m "$mode" "$src" "$dest"
}

say "Installing binaries"
install_file "$NODE_SRC" "$BIN_DIR/ozma-node" 0755
install_file "$VNM_SRC" "$BIN_DIR/ozma-virtual-node" 0755

# ---------------------------------------------------------------------------
say ""
say "Creating state/runtime directories"
for dir in "$STATE_DIR" "$RUNTIME_DIR" "$SUP_STATE_DIR" "$SUP_RUNTIME_DIR"; do
    run mkdir -p "$dir"
done

# ---------------------------------------------------------------------------
# Keep a user unit usable without system-only privilege directives. System
# units are installed unchanged.
render_unit() {
    template=$1
    dest=$2

    content=$(
        sed \
            -e "s|@BIN_DIR@|$BIN_DIR|g" \
            -e "s|@ENV_DIR@|$ENV_DIR|g" \
            -e "s|@STATE_DIR@|$STATE_DIR|g" \
            -e "s|@RUNTIME_DIR@|$RUNTIME_DIR|g" \
            -e "s|@SUPERVISOR_STATE_DIR@|$SUP_STATE_DIR|g" \
            -e "s|@SUPERVISOR_RUNTIME_DIR@|$SUP_RUNTIME_DIR|g" \
            -e "s|@OZMA_USER@|ozma-node|g" \
            -e "s|@OZMA_GROUP@|ozma-node|g" \
            "$template"
    )

    if [ "$MODE" = "user" ]; then
        content=$(
            printf '%s\n' "$content" | grep -Ev \
                '^(User|Group|SupplementaryGroups|AmbientCapabilities|CapabilityBoundingSet|DeviceAllow|ProtectHome|ProtectSystem|PrivateDevices|ReadWritePaths)='
        )
        content=$(printf '%s\n' "$content" | sed 's/^WantedBy=multi-user.target$/WantedBy=default.target/')
    fi

    if [ -e "$dest" ] && [ "$(cat "$dest")" != "$content" ] && [ "$FORCE" != "1" ]; then
        die "$dest exists and differs; re-run with --force to replace it"
    fi

    if [ "$DRY_RUN" = "1" ]; then
        say "DRY   write $dest"
    else
        mkdir -p "$(dirname "$dest")"
        printf '%s\n' "$content" >"$dest"
        say "  wrote $dest"
    fi
}

say ""
say "Installing systemd units"
run mkdir -p "$UNITS_DIR"
for unit in ozma-node@.service ozma-node-vm@.service ozma-virtual-node.service; do
    render_unit "$SCRIPT_DIR/systemd/$unit" "$UNITS_DIR/$unit"
done

# ---------------------------------------------------------------------------
if [ "$INSTALL_ENV" = "1" ]; then
    say ""
    say "Installing environment examples"
    run mkdir -p "$ENV_DIR"
    for env_file in "$SCRIPT_DIR"/systemd/env/*.env.example; do
        base=$(basename "$env_file")
        dest="$ENV_DIR/$base"
        if [ -e "$dest" ]; then
            say "  kept existing $dest"
        else
            if [ "$DRY_RUN" = "1" ]; then
                say "DRY   copy $env_file -> $dest"
            else
                install -m 0640 "$env_file" "$dest"
                say "  wrote $dest"
            fi
        fi
    done
fi

# ---------------------------------------------------------------------------
say ""
if [ "$MODE" = "user" ]; then
    cat <<EOF
Next steps (no sudo, no unit has been started):
  1. Copy and edit an environment example:
       cp "$ENV_DIR"/ozma-node-vm.env.example "$ENV_DIR"/ozma-node-vm.env
  2. Link/enable the unit:
       systemctl --user daemon-reload
       systemctl --user enable --now ozma-node@vm.service
  3. Inspect:
       systemctl --user status ozma-node@vm.service
       journalctl --user -u ozma-node@vm.service -f

Hardware access (video/input/uinput) still requires host udev/group setup.
The Python softnode/node/agent trees are untouched and remain the rollback path.
EOF
else
    cat <<EOF
Next steps (system layout; no unit has been started):
  1. Create/verify the ozma-node system user and state directories:
       useradd --system --home /var/lib/ozma-node --shell /usr/sbin/nologin ozma-node
       install -d -m 0750 -o ozma-node -g ozma-node /var/lib/ozma-node /run/ozma-node
  2. Copy and edit an environment example under /etc/ozma/.
  3. systemctl daemon-reload
     systemctl enable --now ozma-node@hardware.service
     # or: systemctl enable --now ozma-virtual-node.service

The Python softnode/node/agent trees are untouched and remain the rollback path.
EOF
fi
