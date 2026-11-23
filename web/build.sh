#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Build the Rust/WebAssembly Ozma client and generate web/pkg/ for the demo.
#
# Prerequisites:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version <lockfile version> --locked
#
# The exact wasm-bindgen version is in Cargo.lock. The CLI and the crate must
# match or wasm-bindgen prints a schema-version error at runtime.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
cd "${repo_root}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is not on PATH" >&2
  exit 1
fi

if command -v rustup >/dev/null 2>&1; then
  if ! rustup target list --installed | grep -qx 'wasm32-unknown-unknown'; then
    echo "installing the wasm32-unknown-unknown target…"
    rustup target add wasm32-unknown-unknown
  fi
fi

bindgen_version="$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"/\1/p;q}' Cargo.lock)"
if ! command -v wasm-bindgen >/dev/null 2>&1; then
  cat >&2 <<EOF
error: wasm-bindgen-cli is not installed.

Install the exact version used by Cargo.lock (${bindgen_version:-unknown}):

    cargo install wasm-bindgen-cli --version ${bindgen_version:-<version>} --locked

EOF
  exit 1
fi

installed_version="$(wasm-bindgen --version | awk '{print $2}')"
if [ -n "${bindgen_version}" ] && [ "${installed_version}" != "${bindgen_version}" ]; then
  echo "error: wasm-bindgen ${installed_version} does not match Cargo.lock (${bindgen_version})" >&2
  echo "       cargo install wasm-bindgen-cli --version ${bindgen_version} --locked --force" >&2
  exit 1
fi

echo "building ozma-web for wasm32 (web-sys unstable APIs enabled)…"
RUSTFLAGS="--cfg=web_sys_unstable_apis" \
  cargo build -p ozma-web --target wasm32-unknown-unknown --release

wasm="target/wasm32-unknown-unknown/release/ozma_web.wasm"
if [ ! -f "${wasm}" ]; then
  echo "error: ${wasm} was not produced" >&2
  exit 1
fi

mkdir -p "${script_dir}/pkg"
wasm-bindgen --target web --out-dir "${script_dir}/pkg" "${wasm}"

echo
echo "built ${script_dir}/pkg"
echo "run: node ${script_dir#${repo_root}/}/serve.mjs"
