#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Read-only EVDI/Xorg probe for the M2 remote-display spike.
#
# Default mode runs only observations: no package changes, no module loads,
# no writes outside stdout. Commands that would change the machine or require
# root are printed (and labelled [ROOT]) unless you pass --install --yes.
#
# This script does not create a virtual display or capture frames. It tells you
# what is present and what to run next; see docs/remote-display-evdi-spike.md.

set -euo pipefail

MODE="probe"
ASSUME_YES=0

usage() {
  cat <<'EOF'
Usage: scripts/remote-display-spike.sh [--probe] [--install] [--yes] [--help]

  --probe     Read-only system probe (default). Never changes the machine.
  --install   Print the root-required install steps. With --yes and root,
              run the Debian-family apt install and modprobe command.
  --yes       Confirm executing the [ROOT] commands in --install mode.
  --help      Show this help.

Exit status:
  0  probe completed (does not mean EVDI is ready)
  2  EVDI prerequisites are incomplete (probe completed, feature unavailable)
  3  usage or environment error
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --probe) MODE="probe" ;;
    --install) MODE="install" ;;
    --yes) ASSUME_YES=1 ;;
    --help|-h) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 3 ;;
  esac
  shift
done

have() { command -v "$1" >/dev/null 2>&1; }

section() {
  printf '\n== %s ==\n' "$1"
}

# Print a root-required command without running it.
root_hint() {
  printf '  [ROOT] %s\n' "$*"
}

probe_environment() {
  section "Environment"
  printf 'date: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  printf 'host: %s\n' "$(uname -n)"
  printf 'kernel: %s\n' "$(uname -r)"
  printf 'arch: %s\n' "$(uname -m)"
  if [ -r /etc/os-release ]; then
    # Print the human-readable fields without sourcing arbitrary shell code.
    grep -E '^(PRETTY_NAME|ID|VERSION_ID)=' /etc/os-release | sed 's/^/os: /' || true
  else
    echo 'os: /etc/os-release not readable'
  fi
  printf 'session type: %s\n' "${XDG_SESSION_TYPE:-unknown}"
  printf 'display: %s\n' "${DISPLAY:-unset}"
}

probe_dri() {
  section "/dev/dri"
  if [ -d /dev/dri ]; then
    ls -l /dev/dri 2>/dev/null || true
    local cards render
    cards=$(find /dev/dri -maxdepth 1 -name 'card*' 2>/dev/null | wc -l | tr -d ' ')
    render=$(find /dev/dri -maxdepth 1 -name 'renderD*' 2>/dev/null | wc -l | tr -d ' ')
    printf 'card nodes: %s\nrender nodes: %s\n' "$cards" "$render"
    if [ "$cards" -eq 0 ]; then
      echo 'NOTE: no card nodes. Fix the DRM/GPU side before EVDI.'
    fi
  else
    echo 'MISSING: /dev/dri'
  fi
}

probe_evdi() {
  section "EVDI kernel module"
  if [ -d /sys/module/evdi ]; then
    echo 'loaded: yes (/sys/module/evdi)'
    if [ -r /sys/module/evdi/version ]; then
      printf 'version: %s\n' "$(cat /sys/module/evdi/version)"
    fi
  else
    echo 'loaded: no (/sys/module/evdi absent)'
  fi

  if [ -r /proc/modules ] && grep -q '^evdi ' /proc/modules; then
    echo 'present in /proc/modules: yes'
  else
    echo 'present in /proc/modules: no'
  fi

  if have modinfo; then
    if modinfo evdi >/dev/null 2>&1; then
      printf 'modinfo version: %s\n' "$(modinfo -F version evdi 2>/dev/null || echo unknown)"
      printf 'modinfo filename: %s\n' "$(modinfo -F filename evdi 2>/dev/null || echo unknown)"
    else
      echo 'modinfo evdi: not found in the current module tree'
    fi
  else
    echo 'modinfo: not installed (kmod)'
  fi

  if have lsmod; then
    lsmod | grep '^evdi ' || true
  fi
}

probe_libevdi() {
  section "libevdi"
  local found=0
  if have ldconfig; then
    if ldconfig -p 2>/dev/null | grep -i libevdi; then
      found=1
    fi
  fi
  for candidate in /usr/lib/libevdi.so /usr/lib64/libevdi.so \
    /usr/lib/libevdi.so.0 /usr/lib/libevdi.so.1 \
    /usr/local/lib/libevdi.so /usr/local/lib/libevdi.so.1 \
    /usr/lib/x86_64-linux-gnu/libevdi.so \
    /usr/lib/x86_64-linux-gnu/libevdi.so.1 \
    /usr/lib/aarch64-linux-gnu/libevdi.so \
    /usr/lib/aarch64-linux-gnu/libevdi.so.1; do
    if [ -e "$candidate" ]; then
      printf 'found: %s\n' "$candidate"
      found=1
    fi
  done
  if [ "$found" -eq 0 ]; then
    echo 'MISSING: libevdi not found on common loader paths'
  fi

  section "libevdi headers"
  local header_found=0
  for header in /usr/include/evdi/evdi.h /usr/include/libevdi/evdi.h /usr/local/include/evdi.h; do
    if [ -e "$header" ]; then
      printf 'found: %s\n' "$header"
      header_found=1
    fi
  done
  if [ "$header_found" -eq 0 ]; then
    echo 'no evdi.h found (needed to build a spike harness)'
  fi
}

probe_xorg() {
  section "Xorg / modesetting"
  if have Xorg; then
    Xorg -version 2>&1 | sed 's/^/xorg: /' || true
  else
    echo 'Xorg: not installed or not in PATH'
  fi

  if have ps; then
    ps -ef 2>/dev/null | grep '[X]org' | sed 's/^/process: /' || true
  fi

  if have xrandr; then
    echo 'providers:'
    xrandr --listproviders 2>&1 | sed 's/^/  /' || true
    echo 'monitors:'
    xrandr --listmonitors 2>&1 | sed 's/^/  /' || true
  else
    echo 'xrandr: not installed (x11-xserver-utils)'
  fi

  local log=""
  for candidate in /var/log/Xorg.0.log "$HOME/.local/share/xorg/Xorg.0.log"; do
    if [ -r "$candidate" ]; then
      log="$candidate"
      break
    fi
  done
  if [ -n "$log" ]; then
    echo "log: $log"
    grep -iE 'modeset|evdi|provider|output .*connected' "$log" | tail -n 20 | sed 's/^/  /' || true
  else
    echo 'no readable Xorg log found (try root or the session log path)'
  fi
}

probe_packages() {
  section "Packages (informational)"
  if have dpkg; then
    dpkg -l 2>/dev/null | grep -iE 'evdi|displaylink' | sed 's/^/dpkg: /' || echo 'dpkg: no evdi/displaylink packages'
  elif have rpm; then
    rpm -qa 2>/dev/null | grep -iE 'evdi|displaylink' | sed 's/^/rpm: /' || echo 'rpm: no evdi/displaylink packages'
  else
    echo 'no dpkg/rpm found'
  fi

  if have mokutil; then
    printf 'secure boot: %s\n' "$(mokutil --sb-state 2>/dev/null || echo unknown)"
  fi
}

install_steps() {
  section "Install steps (root required; not run unless --install --yes)"
  cat <<'EOF'
Debian/Ubuntu example (package names vary by release; verify first):
EOF
  root_hint "apt-get update"
  root_hint "apt-get install -y evdi-dkms libevdi0"
  root_hint "modprobe evdi"

  if [ "$ASSUME_YES" -eq 1 ]; then
    if [ "$(id -u)" -ne 0 ]; then
      echo 'refusing to run install commands: not root' >&2
      exit 3
    fi
    if ! have apt-get; then
      echo 'refusing to run install commands: apt-get not found; run the distro commands manually' >&2
      exit 3
    fi
    echo
    echo 'Running the [ROOT] Debian-family install command as requested...'
    apt-get update
    if apt-get install -y evdi-dkms libevdi0; then
      :
    else
      echo 'libevdi0 failed; trying libevdi1'
      apt-get install -y libevdi1
    fi
    modprobe evdi || true
    echo 'Install command finished. Re-run with --probe and reboot if the module did not load.'
  else
    echo
    echo 'Dry run only. Re-run with --install --yes as root to execute the apt commands.'
  fi
}

summary() {
  section "Summary"
  local missing=0
  if [ ! -d /dev/dri ]; then
    echo 'missing: /dev/dri'
    missing=1
  fi
  if [ ! -d /sys/module/evdi ]; then
    echo 'missing: evdi kernel module'
    missing=1
  fi
  if ! { have ldconfig && ldconfig -p 2>/dev/null | grep -qi libevdi; }; then
    local lib_found=0
    for candidate in /usr/lib/libevdi.so.1 /usr/lib/libevdi.so.0 \
      /usr/lib64/libevdi.so.1 /usr/lib/x86_64-linux-gnu/libevdi.so.1 \
      /usr/lib/aarch64-linux-gnu/libevdi.so.1; do
      [ -e "$candidate" ] && lib_found=1
    done
    if [ "$lib_found" -eq 0 ]; then
      echo 'missing: libevdi'
      missing=1
    fi
  fi
  if [ ! -d /dev/dri ] || [ "$missing" -eq 1 ]; then
    echo 'EVDI prerequisites are incomplete; ozma-display must report display.unsupported.'
  else
    echo 'EVDI prerequisites look present. This does NOT prove support: run the M2 spike.'
  fi
  cat <<'EOF'

Next steps:
  1. Re-run with --install (dry run) to see the root commands, or install EVDI
     using your distro's current package names.
  2. Build a throwaway libevdi harness, publish the ozma-display EDID, and
     confirm xrandr adopts the connector as an extended output.
  3. Record every version and result in docs/remote-display-evdi-spike.md.
  4. Do not set supported: true in ozma-display until the spike passes on the
     recorded configuration.

The spike never runs automatically and never changes Xorg configuration.
EOF
  if [ "$missing" -eq 1 ]; then
    return 2
  fi
  return 0
}

probe_environment
probe_dri
probe_evdi
probe_libevdi
probe_xorg
probe_packages

if [ "$MODE" = "install" ]; then
  install_steps
fi

set +e
summary
status=$?
set -e
exit "$status"
