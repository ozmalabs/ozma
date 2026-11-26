#!/usr/bin/env bash
set -euo pipefail

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
required=(cargo x86_64-w64-mingw32-gcc wine xvfb-run)
for tool in "${required[@]}"; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf 'Missing required tool: %s\n' "$tool" >&2
    exit 2
  fi
done

cd "$repo_root"
cargo xwin build --release -p ozma-desktop --target x86_64-pc-windows-msvc

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
x86_64-w64-mingw32-gcc -municode -O2 -Wall -Wextra \
  -o "$scratch/ozma-tray-smoke.exe" \
  ozma-desktop/tests/wine_tray_smoke.c

export WINEPREFIX="${WINEPREFIX:-$repo_root/target/ozma-wine-prefix}"
if [[ ! -d "$WINEPREFIX/drive_c/windows" ]]; then
  printf 'Set WINEPREFIX to an initialized Wine prefix before running this smoke.\n' >&2
  exit 2
fi
desktop_exe="Z:${repo_root//\//\\}/target/x86_64-pc-windows-msvc/release/ozma-desktop.exe"
if ! xvfb-run -a wine "$scratch/ozma-tray-smoke.exe" "$desktop_exe" \
  >"$scratch/smoke.log" 2>&1; then
  cat "$scratch/smoke.log" >&2
  exit 1
fi
grep -F 'Tray window found; context popup shown; Quit command exited with code 0.' \
  "$scratch/smoke.log"
