#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Regenerate proto/baseline.bin, the committed protobuf compatibility baseline.
#
# Usage (from anywhere):
#   scripts/update_proto_baseline.sh
#
# Requirements:
#   - buf on PATH (set BUF=/path/to/buf to use another binary)
#
# Output:
#   proto/baseline.bin  # Buf image (FileDescriptorSet-compatible), no source info
#
# IMPORTANT: run this only when establishing an intentional, wire-compatible
# release baseline. `buf breaking` compares the working tree against this
# file, so regenerating it accepts the current schema as the new compatibility
# floor and silently drops the old one. Do not regenerate it to make an
# accidental breaking change pass CI; fix the schema instead (reserve removed
# field numbers/names, keep enum values, do not change wire types).
#
# The baseline is generated with --exclude-source-info so it stays small and
# byte-for-byte reproducible for a given schema and buf version.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASELINE="$ROOT/proto/baseline.bin"
BUF="${BUF:-buf}"

if ! command -v "$BUF" >/dev/null 2>&1; then
  echo "error: buf is not installed (looked for '$BUF')" >&2
  echo "       install buf 1.75.0 or set BUF=/path/to/buf" >&2
  exit 1
fi

if [[ ! -d "$ROOT/proto/ozma/v1" ]]; then
  echo "error: missing proto sources at $ROOT/proto/ozma/v1" >&2
  exit 1
fi

cd "$ROOT"

# Guard against baselining a schema that does not even build or lint.
"$BUF" lint
"$BUF" build --exclude-source-info -o "$BASELINE"

echo "Updated $BASELINE"
echo "  buf:      $("$BUF" --version)"
echo "  sha256:   $(sha256sum "$BASELINE" | awk '{print $1}')"
echo "Commit the new baseline together with the schema change it represents."
