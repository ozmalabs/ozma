#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Lay out the Rust node binaries, systemd units, and environment examples.
#
# This script never uses sudo, never enables/starts a unit, never edits the
# Python softnode/node/agent trees, and never deletes anything. The Python
# implementation remains the rollback path for one release.
#
# The GStreamer WebRTC helper is installed from scripts/webrtc/ into
# <prefix>/share/ozma/webrtc/ so a node does not need a Python tree checkout.
# It still runs under system Python with GStreamer; pass its installed path to
# `ozma-node --webrtc-helper-script` or OZMA_WEBRTC_HELPER_SCRIPT.
#
# The native Rust helper (ozma-webrtc-native) is built from its own manifest and
# lockfile and installed next to ozma-node. It is video-only; select it with
# `--webrtc-native` / OZMA_WEBRTC_NATIVE=true. Use --no-webrtc-native to skip
# it. ozma-webrtc is deliberately not a member of the root workspace.
#
# Usage:
#   scripts/install-rust.sh --build                 # ~/.local user layout
#   scripts/install-rust.sh --from-target target/release
#   scripts/install-rust.sh --system --prefix /usr/local   # run as root
#   scripts/install-rust.sh --dry-run --build
#   scripts/install-rust.sh --build --no-helper     # skip the Python helper
#   scripts/install-rust.sh --build --no-webrtc-native  # skip the Rust helper
#   scripts/install-rust.sh --build --no-multiseat --no-meeting-detect
#
# ozma-multiseat and ozma-meeting-detect are installed when their binaries are
# present (or built); the actual bin names are read from the crate manifests.
# ozma-multiseat also gets ozma-multiseat.service and its env example.
# ozma-meeting-detect has no unit: it runs on demand in a user session.
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
INSTALL_HELPER=1
INSTALL_WEBRTC_NATIVE=1
INSTALL_MULTISEAT=1
INSTALL_MEETING=1
HELPER_DIR=""

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

die() { echo "error: $*" >&2; exit 1; }

# Print "name<TAB>path" for every [[bin]] in a Cargo manifest. The manifests in
# this repo keep one quoted value per line, which is all this needs.
cargo_bins() {
    [ -f "$1" ] || return 0
    awk '
        function val(line) {
            sub(/^[^=]*=[[:space:]]*/, "", line)
            sub(/[[:space:]]*#.*$/, "", line)
            gsub(/^"|"$/, "", line)
            return line
        }
        function flush() {
            if (in_bin && name != "") print name "\t" path
            in_bin = 0; name = ""; path = ""
        }
        /^[[:space:]]*\[\[bin\]\][[:space:]]*$/ { flush(); in_bin = 1; name = ""; path = ""; next }
        /^[[:space:]]*\[/ { flush(); next }
        in_bin && /^[[:space:]]*name[[:space:]]*=/ { name = val($0); next }
        in_bin && /^[[:space:]]*path[[:space:]]*=/ { path = val($0); next }
        END { flush() }
    ' "$1"
}

cargo_package_name() {
    [ -f "$1" ] || return 0
    awk '
        function val(line) {
            sub(/^[^=]*=[[:space:]]*/, "", line)
            sub(/[[:space:]]*#.*$/, "", line)
            gsub(/^"|"$/, "", line)
            return line
        }
        /^[[:space:]]*\[package\][[:space:]]*$/ { in_pkg = 1; next }
        /^[[:space:]]*\[/ { in_pkg = 0 }
        in_pkg && /^[[:space:]]*name[[:space:]]*=/ { print val($0); exit }
    ' "$1"
}

# The meeting detector is the [[bin]] whose name or path mentions "meeting".
# The historical default is used only when that manifest cannot be read.
meeting_bin_name() {
    manifest=$1
    name=""
    if [ -f "$manifest" ]; then
        name=$(cargo_bins "$manifest" |
            awk -F'\t' 'tolower($1) ~ /meeting/ || tolower($2) ~ /meeting/ { print $1; exit }')
        if [ -z "$name" ]; then
            count=$(cargo_bins "$manifest" | grep -c . || true)
            if [ "$count" = "1" ]; then
                name=$(cargo_bins "$manifest" | cut -f1)
            fi
        fi
    fi
    printf '%s\n' "${name:-ozma-meeting-detect}"
}

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
        --no-helper) INSTALL_HELPER=0 ;;
        --no-webrtc-native) INSTALL_WEBRTC_NATIVE=0 ;;
        --no-multiseat) INSTALL_MULTISEAT=0 ;;
        --no-meeting-detect) INSTALL_MEETING=0 ;;
        --helper-dir) shift; HELPER_DIR="${1:-}" ;;
        --helper-dir=*) HELPER_DIR="${1#*=}" ;;
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

HELPER_DIR="${HELPER_DIR:-$PREFIX/share/ozma/webrtc}"

for path in "$PREFIX" "$BIN_DIR" "$UNITS_DIR" "$ENV_DIR" "$STATE_DIR" \
            "$RUNTIME_DIR" "$SUP_STATE_DIR" "$SUP_RUNTIME_DIR" "$HELPER_DIR"; do
    case "$path" in
        *[[:space:]\&\|]*) die "paths containing whitespace, '&', or '|' are not supported: $path" ;;
    esac
done

FROM_TARGET="${FROM_TARGET:-$REPO_ROOT/target/release}"

# Resolve optional binary names from the manifests rather than hardcoding them.
MULTISEAT_BIN=$(cargo_bins "$REPO_ROOT/ozma-multiseat/Cargo.toml" |
    awk -F'\t' 'NR == 1 { print $1; exit }')
[ -n "$MULTISEAT_BIN" ] || MULTISEAT_BIN=$(cargo_package_name "$REPO_ROOT/ozma-multiseat/Cargo.toml")
MULTISEAT_BIN="${MULTISEAT_BIN:-ozma-multiseat}"
MEETING_BIN=$(meeting_bin_name "$REPO_ROOT/ozma-softnode/Cargo.toml")

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
say "  webrtc:      $([ "$INSTALL_HELPER" = "1" ] && echo "$HELPER_DIR/gst_webrtc_worker.py" || echo skipped)"
say "  webrtc-rust: $([ "$INSTALL_WEBRTC_NATIVE" = "1" ] && echo "$BIN_DIR/ozma-webrtc-native (video-only)" || echo skipped)"
say "  multiseat:   $([ "$INSTALL_MULTISEAT" = "1" ] && echo "$BIN_DIR/$MULTISEAT_BIN (if built) + ozma-multiseat.service" || echo skipped)"
say "  meeting:     $([ "$INSTALL_MEETING" = "1" ] && echo "$BIN_DIR/$MEETING_BIN (if built; on demand, no unit)" || echo skipped)"
say "  target:      $FROM_TARGET"
[ "$DRY_RUN" = "1" ] && say "  dry run:     yes (no files are written)"
say ""

if [ "$DO_BUILD" = "1" ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo is required for --build"
    BUILD_PKGS=(ozma-node ozma-softnode)
    [ "$INSTALL_MULTISEAT" = "1" ] && BUILD_PKGS+=(ozma-multiseat)
    BUILD_ARGS=(build --release)
    for pkg in "${BUILD_PKGS[@]}"; do
        BUILD_ARGS+=(-p "$pkg")
    done
    say "Building release binaries (cargo ${BUILD_ARGS[*]})"
    run cargo "${BUILD_ARGS[@]}"
    FROM_TARGET="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release"
fi

# The native helper lives in its own workspace/lockfile so webrtc-rs's
# x25519-dalek requirement cannot disturb the root workspace graph.
if [ "$INSTALL_WEBRTC_NATIVE" = "1" ]; then
    NATIVE_TARGET="${CARGO_TARGET_DIR:-$REPO_ROOT/ozma-webrtc/target}/release"
    if [ "$DO_BUILD" = "1" ]; then
        command -v cargo >/dev/null 2>&1 || die "cargo is required for --build"
        say "Building native WebRTC helper (cargo build --release --manifest-path ozma-webrtc/Cargo.toml)"
        run cargo build --release --manifest-path "$REPO_ROOT/ozma-webrtc/Cargo.toml"
    fi
else
    NATIVE_TARGET=""
fi

NODE_SRC="$FROM_TARGET/ozma-node"
VNM_SRC="$FROM_TARGET/ozma-virtual-node"
if [ "$DO_BUILD" = "1" ] && [ "$DRY_RUN" = "1" ]; then
    # A dry-run build writes nothing, so the release binaries do not exist yet.
    say "  (dry run: assuming --build produces the release binaries)"
else
    [ -x "$NODE_SRC" ] || die "missing executable $NODE_SRC (run with --build)"
    [ -x "$VNM_SRC" ] || die "missing executable $VNM_SRC (run with --build)"
fi

install_file() {
    src=$1
    dest=$2
    mode=$3
    if [ -e "$dest" ] && [ -f "$src" ] && ! cmp -s "$src" "$dest"; then
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

MULTISEAT_INSTALLED=0
MEETING_INSTALLED=0
if [ "$INSTALL_MULTISEAT" = "1" ]; then
    MULTISEAT_SRC="$FROM_TARGET/$MULTISEAT_BIN"
    if [ "$DO_BUILD" = "1" ] || [ -x "$MULTISEAT_SRC" ]; then
        install_file "$MULTISEAT_SRC" "$BIN_DIR/$MULTISEAT_BIN" 0755
        MULTISEAT_INSTALLED=1
    else
        say "  skipped $MULTISEAT_BIN: no executable at $MULTISEAT_SRC (use --build, or --no-multiseat)"
    fi
fi
if [ "$INSTALL_MEETING" = "1" ]; then
    MEETING_SRC="$FROM_TARGET/$MEETING_BIN"
    if [ "$DO_BUILD" = "1" ] || [ -x "$MEETING_SRC" ]; then
        install_file "$MEETING_SRC" "$BIN_DIR/$MEETING_BIN" 0755
        MEETING_INSTALLED=1
    else
        say "  skipped $MEETING_BIN: no executable at $MEETING_SRC (use --build, or --no-meeting-detect)"
    fi
fi
if [ "$INSTALL_MULTISEAT" = "1" ] && [ "$MULTISEAT_INSTALLED" = "0" ]; then
    say "  note: ozma-multiseat.service is installed only with the multiseat binary"
fi
if [ "$INSTALL_MEETING" = "1" ] && [ "$MEETING_INSTALLED" = "0" ]; then
    say "  note: ozma-meeting-detect has no unit; run it on demand in a user session"
fi

if [ "$INSTALL_HELPER" = "1" ]; then
    HELPER_SRC="$REPO_ROOT/scripts/webrtc/gst_webrtc_worker.py"
    [ -f "$HELPER_SRC" ] || die "missing $HELPER_SRC"
    say ""
    say "Installing GStreamer WebRTC helper (Python 3 + GStreamer required at runtime)"
    install_file "$HELPER_SRC" "$HELPER_DIR/gst_webrtc_worker.py" 0644
    say "  pass it to a node with --webrtc-helper python3 --webrtc-helper-script $HELPER_DIR/gst_webrtc_worker.py"
fi

WEBRTC_NATIVE_INSTALLED=0
if [ "$INSTALL_WEBRTC_NATIVE" = "1" ]; then
    if [ -x "$FROM_TARGET/ozma-webrtc-native" ]; then
        NATIVE_SRC="$FROM_TARGET/ozma-webrtc-native"
    else
        NATIVE_SRC="$NATIVE_TARGET/ozma-webrtc-native"
    fi
    say ""
    say "Installing native Rust WebRTC helper (video-only, OpenH264; no Python/GStreamer)"
    if [ "$DO_BUILD" = "1" ] && [ "$DRY_RUN" = "1" ]; then
        # A dry-run build writes nothing; plan the install anyway.
        install_file "$NATIVE_SRC" "$BIN_DIR/ozma-webrtc-native" 0755
        WEBRTC_NATIVE_INSTALLED=1
    elif [ -x "$NATIVE_SRC" ]; then
        install_file "$NATIVE_SRC" "$BIN_DIR/ozma-webrtc-native" 0755
        WEBRTC_NATIVE_INSTALLED=1
        say "  select it with --webrtc-native (or OZMA_WEBRTC_NATIVE=true); audio is not implemented"
    else
        say "  skipped ozma-webrtc-native: no executable at $NATIVE_SRC (use --build, or --no-webrtc-native)"
    fi
fi

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
UNIT_FILES=(ozma-node@.service ozma-node-vm@.service ozma-virtual-node.service)
[ "$MULTISEAT_INSTALLED" = "1" ] && UNIT_FILES+=(ozma-multiseat.service)
for unit in "${UNIT_FILES[@]}"; do
    render_unit "$SCRIPT_DIR/systemd/$unit" "$UNITS_DIR/$unit"
done

# ---------------------------------------------------------------------------
if [ "$INSTALL_ENV" = "1" ]; then
    say ""
    say "Installing environment examples"
    run mkdir -p "$ENV_DIR"
    for env_file in "$SCRIPT_DIR"/systemd/env/*.env.example; do
        base=$(basename "$env_file")
        if [ "$base" = "ozma-multiseat.env.example" ] && [ "$MULTISEAT_INSTALLED" != "1" ]; then
            say "  skipped $base (no $MULTISEAT_BIN installed)"
            continue
        fi
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
if [ "$MULTISEAT_INSTALLED" = "1" ]; then
    MULTISEAT_USER_HINT="       # desktop seats, after editing ozma-multiseat.env:
       # systemctl --user enable --now ozma-multiseat.service"
    MULTISEAT_SYSTEM_HINT="     # desktop seats: systemctl enable --now ozma-multiseat.service"
else
    MULTISEAT_USER_HINT=""
    MULTISEAT_SYSTEM_HINT=""
fi
if [ "$MEETING_INSTALLED" = "1" ]; then
    MEETING_HINT="Meeting detection has no unit; run it on demand:
  $BIN_DIR/$MEETING_BIN --interval 5 [--ics-path FILE]"
else
    MEETING_HINT="Meeting detection is not installed in this pass."
fi
if [ "$WEBRTC_NATIVE_INSTALLED" = "1" ]; then
    WEBRTC_NATIVE_HINT="or set OZMA_WEBRTC_NATIVE=true to use $BIN_DIR/ozma-webrtc-native (video-only)."
else
    WEBRTC_NATIVE_HINT="(the native helper was not installed in this pass)"
fi
if [ "$MODE" = "user" ]; then
    cat <<EOF
Next steps (no sudo, no unit has been started):
  1. Copy and edit an environment example:
       cp "$ENV_DIR"/ozma-node-vm.env.example "$ENV_DIR"/ozma-node-vm.env
  2. Link/enable the unit:
       systemctl --user daemon-reload
       systemctl --user enable --now ozma-node@vm.service
$MULTISEAT_USER_HINT
  3. Inspect:
       systemctl --user status ozma-node@vm.service
       journalctl --user -u ozma-node@vm.service -f

Optional WebRTC: set OZMA_WEBRTC_HELPER=python3 and
OZMA_WEBRTC_HELPER_SCRIPT="$HELPER_DIR/gst_webrtc_worker.py" in the env file,
$WEBRTC_NATIVE_HINT
$MEETING_HINT
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
$MULTISEAT_SYSTEM_HINT

Optional WebRTC: set OZMA_WEBRTC_HELPER=python3 and
OZMA_WEBRTC_HELPER_SCRIPT="$HELPER_DIR/gst_webrtc_worker.py",
$WEBRTC_NATIVE_HINT

$MEETING_HINT
The Python softnode/node/agent trees are untouched and remain the rollback path.
EOF
fi
