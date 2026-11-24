#!/usr/bin/env sh
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Check the build-time native requirements for the Rust node without sudo.
# Prints PASS/FAIL/SKIP for each requirement and exits non-zero when a required
# dependency is missing. Runtime-only tools are reported as SKIP.
#
# Usage:
#   scripts/clean-host/check-deps.sh              # host check
#   scripts/clean-host/check-deps.sh --container  # same, wording only
#
# Exit status: 0 all required dependencies present; 1 a required one is missing.

set -u

MODE=host
case "${1:-}" in
    --container) MODE=container ;;
    "" ) ;;
    -h|--help)
        sed -n '2,14p' "$0"
        exit 0
        ;;
    *)
        echo "unknown argument: $1" >&2
        exit 2
        ;;
esac

PASS=0
FAIL=0
SKIP=0

pass() { PASS=$((PASS + 1)); printf 'PASS  %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf 'FAIL  %s\n' "$*" >&2; }
skip() { SKIP=$((SKIP + 1)); printf 'SKIP  %s\n' "$*"; }

need_cmd() {
    if command -v "$1" >/dev/null 2>&1; then
        pass "command: $1 ($(command -v "$1"))"
    else
        fail "command: $1 is missing"
    fi
}

need_pc() {
    name=$1
    description=$2
    if ! command -v pkg-config >/dev/null 2>&1; then
        fail "pkg-config is missing, cannot check $description"
        return
    fi
    if pkg-config --exists "$name"; then
        version=$(pkg-config --modversion "$name" 2>/dev/null || true)
        pass "pkg-config: $name${version:+ $version} ($description)"
    else
        fail "pkg-config: $name is missing ($description)"
    fi
}

has_libclang() {
    if command -v clang >/dev/null 2>&1; then
        return 0
    fi
    if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists libclang 2>/dev/null; then
        return 0
    fi
    for candidate in /usr/lib/llvm-*/lib/libclang.so* /usr/lib64/libclang.so* /usr/lib/libclang.so*; do
        [ -e "$candidate" ] && return 0
    done
    return 1
}

optional_cmd() {
    if command -v "$1" >/dev/null 2>&1; then
        pass "optional: $1 ($(command -v "$1"))"
    else
        skip "optional: $1 not found ($2)"
    fi
}

echo "Ozma clean-host dependency check ($MODE)"
echo "-----------------------------------------"

need_cmd cargo
need_cmd rustc
need_cmd cc
need_cmd pkg-config

# Toolchain version is informational; Cargo.lock decides crate versions.
if command -v rustc >/dev/null 2>&1; then
    echo "INFO  rustc: $(rustc --version)"
fi
if command -v cargo >/dev/null 2>&1; then
    echo "INFO  cargo: $(cargo --version)"
fi
echo ""

need_pc libudev "udev/hotplug (ozma-drivers, usb-gadget)"
need_pc alsa "ALSA capture/playback (ozma-audio, ozma-playback)"
need_pc libpipewire-0.3 "PipeWire audio/video (ozma-pipewire, ozma-capture)"
need_pc pam "PAM link-time dependency (ozma-node)"

if has_libclang; then
    if command -v clang >/dev/null 2>&1; then
        pass "libclang: clang $(clang --version | head -n1)"
    else
        pass "libclang: found via pkg-config or /usr/lib"
    fi
else
    fail "libclang/clang is missing (bindgen for v4l2-sys-mit)"
fi

echo ""
optional_cmd ffmpeg "runtime HLS/VNC/X11 capture"
optional_cmd qemu-system-x86_64 "live VM demo"
optional_cmd virsh "libvirt power/provisioning"
optional_cmd pactl "PipeWire/PulseAudio routing"
optional_cmd systemd-analyze "unit-file verification"
optional_cmd shellcheck "shell lint"

echo ""
echo "Summary: $PASS passed, $FAIL failed, $SKIP skipped"

if [ "$FAIL" -ne 0 ]; then
    cat >&2 <<'EOF'

Install the missing build dependencies without changing the Ozma tree.

Debian/Ubuntu:
  sudo apt-get install pkg-config build-essential libudev-dev libasound2-dev \
      libpipewire-0.3-dev libpam0g-dev libclang-dev clang cmake nasm ffmpeg

Fedora/RHEL:
  sudo dnf install pkgconf-pkg-config gcc gcc-c++ systemd-devel alsa-lib-devel \
      pipewire-devel pam-devel clang clang-devel cmake nasm ffmpeg

Arch:
  sudo pacman -S --needed pkgconf base-devel systemd-libs alsa-lib pipewire pam \
      clang cmake nasm ffmpeg

No-sudo alternative for this repository:
  scripts/clean-host/run.sh --container
EOF
    exit 1
fi

exit 0
