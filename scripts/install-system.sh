#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Install host-level system integration for the Rust Ozma node:
# dedicated user/group, state and runtime directories, systemd units, env
# examples, udev rules, the AppArmor libvirt-qemu drop-in, a copy-only libvirt
# cgroup_device_acl snippet, tmpfiles/modules-load snippets, and the opt-in USB
# gadget drop-in.
#
# This script never invokes sudo, never enables or starts a unit, and never
# edits /etc/libvirt/qemu.conf. Run it as root for a live system, or use
# --root DIR to render a rootless temp-prefix layout for review/testing.
#
# Usage:
#   sudo scripts/install-system.sh --profile hardware
#   sudo scripts/install-system.sh --profile all --usb-gadget
#   sudo scripts/install-system.sh --dry-run --profile vm
#   scripts/install-system.sh --root /tmp/ozma-system --profile all
#
# Exit status: 0 success; 1 missing tool/refused overwrite/privilege error;
# 2 usage error.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SYSTEM_DIR="$SCRIPT_DIR/system"
cd "$REPO_ROOT"

ROOT=""
PREFIX="/usr/local"
PROFILES="hardware"
USB_GADGET=0
DRY_RUN=0
FORCE=0
RELOAD=0
DO_USER=1
DO_DIRS=1
DO_UNITS=1
DO_ENV=1
DO_UDEV=1
DO_APPARMOR=1
DO_LIBVIRT=1
DO_TMPFILES=1
DO_MODULES=1
SYS_USER="ozma-node"
SYS_GROUP="ozma-node"

UDEV_DIR=""
APPARMOR_PARENT=""
LIBVIRT_CONF=""
TMPFILES_DIR=""
MODULES_DIR=""

usage() {
    sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
}

die() { echo "error: $*" >&2; exit 1; }
die_usage() { echo "error: $*" >&2; exit 2; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --root) shift; ROOT="${1:-}" ;;
        --root=*) ROOT="${1#*=}" ;;
        --prefix) shift; PREFIX="${1:-}" ;;
        --prefix=*) PREFIX="${1#*=}" ;;
        --profile) shift; PROFILES="${1:-}" ;;
        --profile=*) PROFILES="${1#*=}" ;;
        --user) shift; SYS_USER="${1:-}" ;;
        --user=*) SYS_USER="${1#*=}" ;;
        --group) shift; SYS_GROUP="${1:-}" ;;
        --group=*) SYS_GROUP="${1#*=}" ;;
        --udev-dir) shift; UDEV_DIR="${1:-}" ;;
        --udev-dir=*) UDEV_DIR="${1#*=}" ;;
        --apparmor-dir) shift; APPARMOR_PARENT="${1:-}" ;;
        --apparmor-dir=*) APPARMOR_PARENT="${1#*=}" ;;
        --libvirt-conf) shift; LIBVIRT_CONF="${1:-}" ;;
        --libvirt-conf=*) LIBVIRT_CONF="${1#*=}" ;;
        --tmpfiles-dir) shift; TMPFILES_DIR="${1:-}" ;;
        --tmpfiles-dir=*) TMPFILES_DIR="${1#*=}" ;;
        --modules-dir) shift; MODULES_DIR="${1:-}" ;;
        --modules-dir=*) MODULES_DIR="${1#*=}" ;;
        --usb-gadget) USB_GADGET=1 ;;
        --no-usb-gadget) USB_GADGET=0 ;;
        --no-user) DO_USER=0 ;;
        --no-dirs) DO_DIRS=0 ;;
        --no-units) DO_UNITS=0 ;;
        --no-env) DO_ENV=0 ;;
        --no-udev) DO_UDEV=0 ;;
        --no-apparmor) DO_APPARMOR=0 ;;
        --no-libvirt) DO_LIBVIRT=0 ;;
        --no-tmpfiles) DO_TMPFILES=0 ;;
        --no-modules) DO_MODULES=0 ;;
        --reload) RELOAD=1 ;;
        --dry-run) DRY_RUN=1 ;;
        --force) FORCE=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
# Profiles: hardware, vm, desktop, or all (comma- or space-separated).
PROFILES="${PROFILES//,/ }"
NORMALIZED=""
for p in $PROFILES; do
    case "$p" in
        all) NORMALIZED="hardware vm desktop" ;;
        hardware|vm|desktop) NORMALIZED="${NORMALIZED:+$NORMALIZED }$p" ;;
        *) die_usage "unknown profile '$p'; expected hardware, vm, desktop, or all" ;;
    esac
done
[ -n "$NORMALIZED" ] || die_usage "at least one profile is required"
# De-duplicate while preserving order.
PROFILES=""
for p in $NORMALIZED; do
    case " $PROFILES " in
        *" $p "*) ;;
        *) PROFILES="${PROFILES:+$PROFILES }$p" ;;
    esac
done

want_profile() {
    case " $PROFILES " in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
    esac
}

# ---------------------------------------------------------------------------
# Root versus staging. A non-empty --root is a temp-prefix/staging install:
# assets are rendered there and host account/database changes are recorded as
# operator commands instead of executed.
ROOT="${ROOT%/}"
[ "$ROOT" = "/" ] && ROOT=""
STAGING=0
[ -n "$ROOT" ] && STAGING=1
IS_ROOT=0
[ "$(id -u)" -eq 0 ] && IS_ROOT=1

PREFIX="${PREFIX%/}"
[ -n "$PREFIX" ] || PREFIX="/"
case "$PREFIX" in
    /*) ;;
    *) die_usage "--prefix must be an absolute path (for example /usr/local)" ;;
esac
case "$ROOT" in
    ""|/*) ;;
    *) die_usage "--root must be an absolute path" ;;
esac
case "$ROOT" in
    *[[:space:]\&\|]*) die_usage "--root may not contain whitespace, '&', or '|'" ;;
esac
case "$PREFIX" in
    *[[:space:]\&\|]*) die_usage "--prefix may not contain whitespace, '&', or '|'" ;;
esac
if [ "$STAGING" -eq 0 ] && [ "$IS_ROOT" -ne 1 ] && [ "$DRY_RUN" -ne 1 ]; then
    die "a live system install requires root; use --root DIR for a rootless temp-prefix test, or --dry-run"
fi

# ---------------------------------------------------------------------------
# Paths. ROOT is empty for a live install, so these are the real paths; with
# --root they are all staged under that directory.
BIN_DIR="$ROOT$PREFIX/bin"
UNIT_DIR="$ROOT$PREFIX/lib/systemd/system"
ENV_DIR="$ROOT/etc/ozma"
STATE_DIR="$ROOT/var/lib/ozma-node"
RUNTIME_DIR="$ROOT/run/ozma-node"
SUP_STATE_DIR="$ROOT/var/lib/ozma"
SUP_RUNTIME_DIR="$ROOT/run/ozma"
QMP_DIR="$ROOT/run/ozma/qmp"
UDEV_DIR="${UDEV_DIR:-$ROOT/etc/udev/rules.d}"
APPARMOR_PARENT="${APPARMOR_PARENT:-$ROOT/etc/apparmor.d/abstractions/libvirt-qemu.d}"
LIBVIRT_CONF="${LIBVIRT_CONF:-$ROOT/etc/libvirt/qemu.conf}"
TMPFILES_DIR="${TMPFILES_DIR:-$ROOT/etc/tmpfiles.d}"
MODULES_DIR="${MODULES_DIR:-$ROOT/etc/modules-load.d}"
LIBVIRT_SNIPPET="$ENV_DIR/libvirt-qemu-cgroup_device_acl.conf"
OPERATOR_FILE="$ENV_DIR/install-operator-commands.txt"
REAL_STATE_DIR="/var/lib/ozma-node"

# ---------------------------------------------------------------------------
RENDER_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ozma-system-install.XXXXXX")
cleanup() { rm -rf "$RENDER_DIR"; }
trap cleanup EXIT

OP_CMDS=()
say() { printf '%s\n' "$*"; }

group_exists() { getent group "$1" >/dev/null 2>&1; }
user_exists() { getent passwd "$1" >/dev/null 2>&1; }

QMP_GROUP="kvm"
group_exists kvm || QMP_GROUP="root"

SUPP_GROUPS=""
add_supp_group() {
    group_exists "$1" || return 0
    case ",$SUPP_GROUPS," in
        *",$1,"*) return 0 ;;
    esac
    SUPP_GROUPS="${SUPP_GROUPS:+$SUPP_GROUPS,}$1"
}
if want_profile hardware; then
    add_supp_group video
    add_supp_group input
    add_supp_group render
fi
if want_profile vm; then
    add_supp_group input
    add_supp_group render
    add_supp_group kvm
    add_supp_group libvirt
fi
if want_profile desktop; then
    add_supp_group input
    add_supp_group video
    add_supp_group render
fi

record_cmd() { OP_CMDS+=("$*"); }

run_privileged() {
    if [ "$DRY_RUN" -eq 1 ]; then
        printf 'DRY   %s\n' "$*"
        return 0
    fi
    if [ "$IS_ROOT" -eq 1 ] && [ "$STAGING" -eq 0 ]; then
        "$@"
    else
        record_cmd "$*"
        printf 'SKIP  %s (requires root; recorded in %s)\n' "$*" "$OPERATOR_FILE"
    fi
}

owner_ok() { user_exists "$1" && group_exists "$2"; }

install_dir() {
    local dir=$1 mode=$2 owner=$3 group=$4
    if [ "$DRY_RUN" -eq 1 ]; then
        printf 'DIR   %s (mode %s)\n' "$dir" "$mode"
        return 0
    fi
    if [ "$IS_ROOT" -eq 1 ] && [ "$STAGING" -eq 0 ] && owner_ok "$owner" "$group"; then
        install -d -m "$mode" -o "$owner" -g "$group" "$dir"
    else
        install -d -m "$mode" "$dir"
    fi
}

install_prepared() {
    local src=$1 dest=$2 mode=$3 owner=$4 group=$5
    if [ -e "$dest" ] && ! cmp -s "$src" "$dest"; then
        if [ "$FORCE" -ne 1 ]; then
            die "$dest exists and differs; re-run with --force to replace it"
        fi
    fi
    if [ "$DRY_RUN" -eq 1 ]; then
        printf 'FILE  %s (mode %s)\n' "$dest" "$mode"
        return 0
    fi
    if [ "$IS_ROOT" -eq 1 ] && [ "$STAGING" -eq 0 ] && owner_ok "$owner" "$group"; then
        install -m "$mode" -o "$owner" -g "$group" "$src" "$dest"
    else
        install -m "$mode" "$src" "$dest"
    fi
}

render_template() {
    local src=$1 dest=$2
    sed \
        -e "s|@OZMA_USER@|$SYS_USER|g" \
        -e "s|@OZMA_GROUP@|$SYS_GROUP|g" \
        -e "s|@QMP_GROUP@|$QMP_GROUP|g" \
        -e "s|@USB_GADGET_MODULE@|$USB_GADGET_MODULE|g" \
        -e "s|@BIN_DIR@|$BIN_DIR|g" \
        -e "s|@ENV_DIR@|$ENV_DIR|g" \
        -e "s|@STATE_DIR@|$STATE_DIR|g" \
        -e "s|@RUNTIME_DIR@|$RUNTIME_DIR|g" \
        -e "s|@SUPERVISOR_STATE_DIR@|$SUP_STATE_DIR|g" \
        -e "s|@SUPERVISOR_RUNTIME_DIR@|$SUP_RUNTIME_DIR|g" \
        "$src" >"$dest"
}

install_rendered() {
    local src=$1 dest=$2 mode=$3 owner=$4 group=$5
    local name
    name="$(basename "$(dirname "$dest")")-$(basename "$dest").rendered"
    render_template "$src" "$RENDER_DIR/$name"
    install_prepared "$RENDER_DIR/$name" "$dest" "$mode" "$owner" "$group"
}

# ---------------------------------------------------------------------------
say "Ozma Rust node system install"
say "  root:        ${ROOT:-/ (live)}"
say "  prefix:      $PREFIX"
say "  profiles:    $PROFILES"
say "  user/group:  $SYS_USER:$SYS_GROUP"
say "  units:       $UNIT_DIR"
say "  environment: $ENV_DIR"
say "  state:       $STATE_DIR $SUP_STATE_DIR"
say "  runtime:     $RUNTIME_DIR $SUP_RUNTIME_DIR"
say "  udev:        $UDEV_DIR"
say "  apparmor:    $APPARMOR_PARENT"
say "  libvirt:     snippet only ($LIBVIRT_SNIPPET); qemu.conf is never edited"
say "  usb gadget:  $([ "$USB_GADGET" -eq 1 ] && echo yes || echo no)"
say "  reload:      $([ "$RELOAD" -eq 1 ] && echo yes || echo no)"
[ "$DRY_RUN" -eq 1 ] && say "  dry run:     yes (no files are written)"
[ "$STAGING" -eq 1 ] && say "  staging:     yes (host account changes are recorded, not run)"
say ""

USB_GADGET_MODULE=""
[ "$USB_GADGET" -eq 1 ] && USB_GADGET_MODULE="libcomposite"
if [ "$USB_GADGET" -eq 1 ] && ! want_profile hardware; then
    say "NOTE  --usb-gadget is normally paired with --profile hardware; installing the gadget assets anyway."
fi

# ---------------------------------------------------------------------------
# 1. System user and group.
say "System user and group"
if [ "$DO_USER" -eq 1 ]; then
    if group_exists "$SYS_GROUP"; then
        say "  group $SYS_GROUP exists"
    else
        run_privileged groupadd --system "$SYS_GROUP"
    fi
    if user_exists "$SYS_USER"; then
        say "  user $SYS_USER exists"
    else
        run_privileged useradd --system --gid "$SYS_GROUP" \
            --home-dir "$REAL_STATE_DIR" --shell /usr/sbin/nologin \
            --no-create-home "$SYS_USER"
    fi
    if [ -n "$SUPP_GROUPS" ]; then
        run_privileged usermod -aG "$SUPP_GROUPS" "$SYS_USER"
        say "  supplementary groups: $SUPP_GROUPS"
    else
        say "  supplementary groups: none present on this host"
    fi
else
    say "  skipped (--no-user)"
fi

# ---------------------------------------------------------------------------
# 2. Directories.
if [ "$DO_DIRS" -eq 1 ]; then
    say ""
    say "Creating state, runtime, and configuration directories"
    install_dir "$ENV_DIR" 0750 root "$SYS_GROUP"
    install_dir "$STATE_DIR" 0750 "$SYS_USER" "$SYS_GROUP"
    install_dir "$STATE_DIR/hls" 0750 "$SYS_USER" "$SYS_GROUP"
    install_dir "$SUP_STATE_DIR" 0750 "$SYS_USER" "$SYS_GROUP"
    install_dir "$RUNTIME_DIR" 0750 "$SYS_USER" "$SYS_GROUP"
    install_dir "$SUP_RUNTIME_DIR" 0750 "$SYS_USER" "$SYS_GROUP"
    install_dir "$QMP_DIR" 0770 root "$QMP_GROUP"
fi

# ---------------------------------------------------------------------------
# 3. Systemd units.
if [ "$DO_UNITS" -eq 1 ]; then
    say ""
    say "Installing systemd units"
    install_dir "$UNIT_DIR" 0755 root root
    for unit in ozma-node@.service ozma-node-vm@.service ozma-virtual-node.service; do
        install_rendered "$SCRIPT_DIR/systemd/$unit" "$UNIT_DIR/$unit" 0644 root root
        say "  $UNIT_DIR/$unit"
    done
    if [ "$USB_GADGET" -eq 1 ]; then
        install_dir "$UNIT_DIR/ozma-node@.service.d" 0755 root root
        install_rendered "$SYSTEM_DIR/systemd/10-usb-gadget.conf.in" \
            "$UNIT_DIR/ozma-node@.service.d/10-usb-gadget.conf" 0644 root root
        say "  $UNIT_DIR/ozma-node@.service.d/10-usb-gadget.conf"
    fi
fi

# ---------------------------------------------------------------------------
# 4. Environment examples (never overwrite an existing file).
if [ "$DO_ENV" -eq 1 ]; then
    say ""
    say "Installing environment examples"
    install_dir "$ENV_DIR" 0750 root "$SYS_GROUP"
    for src in "$SCRIPT_DIR"/systemd/env/*.env.example; do
        dest="$ENV_DIR/$(basename "$src")"
        if [ -e "$dest" ]; then
            say "  kept existing $dest"
        elif [ "$DRY_RUN" -eq 1 ]; then
            printf 'FILE  %s (mode 0640)\n' "$dest"
        else
            if [ "$IS_ROOT" -eq 1 ] && [ "$STAGING" -eq 0 ] && owner_ok root "$SYS_GROUP"; then
                install -m 0640 -o root -g "$SYS_GROUP" "$src" "$dest"
            else
                install -m 0640 "$src" "$dest"
            fi
            say "  $dest"
        fi
    done
fi

# ---------------------------------------------------------------------------
# 5. udev rules.
if [ "$DO_UDEV" -eq 1 ]; then
    say ""
    say "Installing udev rules"
    install_dir "$UDEV_DIR" 0755 root root
    install_udev() {
        local src=$1 name=$2
        install_rendered "$src" "$UDEV_DIR/$name" 0644 root root
        say "  $UDEV_DIR/$name"
    }
    if want_profile vm || want_profile desktop; then
        install_udev "$SYSTEM_DIR/udev/70-ozma-node-uinput.rules.in" 70-ozma-node-uinput.rules
        install_udev "$SYSTEM_DIR/udev/70-ozma-node-input.rules.in" 70-ozma-node-input.rules
    fi
    if want_profile hardware || [ "$USB_GADGET" -eq 1 ]; then
        install_udev "$SYSTEM_DIR/udev/70-ozma-node-hidg.rules.in" 70-ozma-node-hidg.rules
    fi
    if want_profile hardware; then
        install_udev "$SYSTEM_DIR/udev/70-ozma-node-v4l2.rules.in" 70-ozma-node-v4l2.rules
    fi
    install_udev "$SYSTEM_DIR/udev/70-ozma-node-render.rules.in" 70-ozma-node-render.rules
    if [ "$USB_GADGET" -eq 1 ]; then
        install_udev "$SYSTEM_DIR/udev/70-ozma-node-gadget.rules.in" 70-ozma-node-gadget.rules
    fi
fi

# ---------------------------------------------------------------------------
# 6. AppArmor drop-in.
if [ "$DO_APPARMOR" -eq 1 ]; then
    say ""
    say "Installing AppArmor libvirt-qemu drop-in"
    install_dir "$APPARMOR_PARENT" 0755 root root
    install_prepared "$SYSTEM_DIR/apparmor/libvirt-qemu.d-ozma" \
        "$APPARMOR_PARENT/ozma" 0644 root root
    say "  $APPARMOR_PARENT/ozma"
fi

# ---------------------------------------------------------------------------
# 7. libvirt cgroup_device_acl snippet (copy only, never edit qemu.conf).
if [ "$DO_LIBVIRT" -eq 1 ]; then
    say ""
    say "Installing libvirt cgroup_device_acl snippet (guidance only)"
    install_dir "$ENV_DIR" 0750 root "$SYS_GROUP"
    install_prepared "$SYSTEM_DIR/libvirt/qemu-cgroup_device_acl.conf.snippet" \
        "$LIBVIRT_SNIPPET" 0640 root "$SYS_GROUP"
    say "  $LIBVIRT_SNIPPET"
    if [ -f "$LIBVIRT_CONF" ]; then
        if grep -qE '^[[:space:]]*cgroup_device_acl[[:space:]]*=' "$LIBVIRT_CONF"; then
            say "  NOTE  $LIBVIRT_CONF already has cgroup_device_acl; merge only the"
            say "        input event entries if they are missing, then restart libvirtd."
        else
            say "  NOTE  $LIBVIRT_CONF has no cgroup_device_acl; append the snippet"
            say "        manually, then restart libvirtd."
        fi
    else
        say "  NOTE  $LIBVIRT_CONF not found; keep the snippet for a later libvirt install."
    fi
fi

# ---------------------------------------------------------------------------
# 8. tmpfiles and modules-load snippets.
if [ "$DO_TMPFILES" -eq 1 ]; then
    say ""
    say "Installing tmpfiles snippet"
    install_dir "$TMPFILES_DIR" 0755 root root
    install_rendered "$SYSTEM_DIR/tmpfiles/ozma-node.conf.in" \
        "$TMPFILES_DIR/ozma-node.conf" 0644 root root
    say "  $TMPFILES_DIR/ozma-node.conf"
fi
if [ "$DO_MODULES" -eq 1 ]; then
    say ""
    say "Installing modules-load snippet"
    install_dir "$MODULES_DIR" 0755 root root
    install_rendered "$SYSTEM_DIR/modules-load/ozma-node.conf.in" \
        "$MODULES_DIR/ozma-node.conf" 0644 root root
    say "  $MODULES_DIR/ozma-node.conf"
fi

# ---------------------------------------------------------------------------
# 9. Optional reload of host state (never enable/start a unit).
if [ "$RELOAD" -eq 1 ]; then
    say ""
    say "Applying reloads (no unit is enabled or started)"
    if [ "$DRY_RUN" -eq 1 ] || [ "$STAGING" -eq 1 ]; then
        say "  reload skipped in staging/dry-run"
    else
        run_privileged systemctl daemon-reload
        [ "$DO_UDEV" -eq 1 ] && run_privileged udevadm control --reload-rules
        [ "$DO_TMPFILES" -eq 1 ] && run_privileged systemd-tmpfiles --create "$TMPFILES_DIR/ozma-node.conf"
        if [ "$DO_APPARMOR" -eq 1 ] && systemctl is-active --quiet apparmor 2>/dev/null; then
            run_privileged systemctl reload apparmor
        fi
    fi
fi

# ---------------------------------------------------------------------------
# 10. Operator commands file (exact commands that were not run here).
if [ "$DRY_RUN" -eq 0 ]; then
    OP_RENDER="$RENDER_DIR/install-operator-commands.txt"
    {
        echo "# Ozma system install operator commands"
        echo "# Generated by scripts/install-system.sh. Run these as root."
        echo "# Profiles: $PROFILES"
        echo "# Staging root: ${ROOT:-/ (live)}"
        echo ""
        if [ "${#OP_CMDS[@]}" -gt 0 ]; then
            echo "# Commands skipped by this pass:"
            for cmd in "${OP_CMDS[@]}"; do
                echo "$cmd"
            done
            echo ""
        fi
        echo "# Apply host state (no unit is enabled or started):"
        echo "systemctl daemon-reload"
        if [ "$DO_UDEV" -eq 1 ]; then
            echo "udevadm control --reload-rules"
            echo "udevadm trigger --action=change"
        fi
        if [ "$DO_TMPFILES" -eq 1 ]; then
            echo "systemd-tmpfiles --create $TMPFILES_DIR/ozma-node.conf"
        fi
        if [ "$DO_APPARMOR" -eq 1 ]; then
            echo "systemctl reload apparmor    # only if AppArmor is active"
        fi
        if [ "$DO_LIBVIRT" -eq 1 ]; then
            echo "# Merge $LIBVIRT_SNIPPET into /etc/libvirt/qemu.conf, then:"
            echo "systemctl restart libvirtd"
        fi
        echo ""
        echo "# Enable only the profile(s) you intend to run:"
        if want_profile hardware; then
            echo "systemctl enable --now ozma-node@hardware.service"
        fi
        if want_profile vm; then
            echo "systemctl enable --now ozma-node@vm.service"
            echo "systemctl enable --now ozma-virtual-node.service    # supervisor alternative"
            echo "systemctl enable --now ozma-node-vm@<vm-name>.service"
        fi
        if want_profile desktop; then
            echo "# Desktop: prefer a systemd --user unit (see docs/clean-host.md)."
            echo "systemctl enable --now ozma-node@desktop.service    # only with explicit session access"
        fi
        echo ""
        echo "# Rollback:"
        for unit in ozma-node@hardware.service ozma-node@vm.service ozma-node@desktop.service \
                    ozma-virtual-node.service 'ozma-node-vm@<vm-name>.service'; do
            echo "systemctl disable --now $unit    # if enabled"
        done
        echo "rm -f $UNIT_DIR/ozma-node@.service $UNIT_DIR/ozma-node-vm@.service $UNIT_DIR/ozma-virtual-node.service"
        echo "rm -rf $UNIT_DIR/ozma-node@.service.d"
        echo "rm -f $UDEV_DIR/70-ozma-node-*.rules"
        echo "rm -f $APPARMOR_PARENT/ozma"
        echo "rm -f $TMPFILES_DIR/ozma-node.conf $MODULES_DIR/ozma-node.conf"
        echo "rm -f $LIBVIRT_SNIPPET"
        echo "# Keep $ENV_DIR if it contains tokens; remove it only deliberately."
        echo "# userdel $SYS_USER; groupdel $SYS_GROUP    # only if no other use"
    } >"$OP_RENDER"
    install_prepared "$OP_RENDER" "$OPERATOR_FILE" 0640 root "$SYS_GROUP"
    say ""
    say "Wrote operator commands to $OPERATOR_FILE"
fi

# ---------------------------------------------------------------------------
# 11. Next steps. Never enable or start anything here.
if [ "$DRY_RUN" -eq 0 ]; then
    say ""
    if [ ! -x "$BIN_DIR/ozma-node" ]; then
        say "NOTE  $BIN_DIR/ozma-node is not installed yet."
        say "      Install the binaries and re-render the units with:"
        say "        scripts/install-rust.sh --from-target target/release --system --prefix $PREFIX"
    fi
    say "No unit was enabled or started. Exact commands are in:"
    say "  $OPERATOR_FILE"
    say ""
    say "Enable a profile only after editing its env file:"
    if want_profile hardware; then
        say "  systemctl enable --now ozma-node@hardware.service"
    fi
    if want_profile vm; then
        say "  systemctl enable --now ozma-node@vm.service"
        say "  # or: systemctl enable --now ozma-virtual-node.service"
    fi
    if want_profile desktop; then
        say "  # Desktop: use a systemd --user unit where possible; a system unit"
        say "  # cannot see the logged-in user's PipeWire session by default."
    fi
    if [ "$USB_GADGET" -eq 1 ]; then
        say "USB gadget is opt-in and needs a mounted ConfigFS root, a UDC, and"
        say "operator-owned gadget setup; see docs/clean-host.md."
    fi
fi
