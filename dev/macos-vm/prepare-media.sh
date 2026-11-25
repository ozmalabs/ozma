#!/usr/bin/env bash
# Prepare offline self-bootstrap media for the emulated macOS VM (or a Mac).
#
# Stages:
#   - Rust workspace source (ozma-node + all workspace crates + proto/)
#   - Vendored crates (cargo vendor) for offline compilation
#   - install-offline.sh (runs cargo test -p ozma-input, builds ozma-node
#     --release, installs /usr/local/bin/ozma-node + LaunchDaemon plist)
#
# Usage:
#   CONTROLLER_URL=http://10.0.2.2:7380 bash dev/macos-vm/prepare-media.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
MEDIA_DIR="${MEDIA_DIR:-$REPO_DIR/images/ozma-media-macos}"
ISO_PATH="${ISO_PATH:-$REPO_DIR/images/ozma-media-macos.img}"
CONTROLLER_URL="${CONTROLLER_URL:-http://10.0.2.2:7380}"
SKIP_VENDOR="${SKIP_VENDOR:-0}"

echo "=== Preparing offline media for macOS Rust node (ozma-node) ==="
echo "Repo dir:       $REPO_DIR"
echo "Media dir:      $MEDIA_DIR"
echo "Controller URL: $CONTROLLER_URL"

SRC_DIR="$MEDIA_DIR/ozma/src"
mkdir -p "$SRC_DIR" "$MEDIA_DIR/ozma"

rm -rf "$SRC_DIR"/ozma-* "$SRC_DIR"/softnode-qmp "$SRC_DIR"/proto "$SRC_DIR"/protocol 2>/dev/null || true
cp "$REPO_DIR/Cargo.toml" "$SRC_DIR/Cargo.toml"
cp "$REPO_DIR/Cargo.lock" "$SRC_DIR/Cargo.lock"
[ -f "$REPO_DIR/build.rs" ] && cp "$REPO_DIR/build.rs" "$SRC_DIR/build.rs"
[ -d "$REPO_DIR/proto" ] && cp -r "$REPO_DIR/proto" "$SRC_DIR/proto"
[ -d "$REPO_DIR/protocol" ] && cp -r "$REPO_DIR/protocol" "$SRC_DIR/protocol"

for crate_dir in "$REPO_DIR"/ozma-* "$REPO_DIR"/softnode-qmp; do
    [ -d "$crate_dir" ] || continue
    [ -f "$crate_dir/Cargo.toml" ] || continue
    crate_name="$(basename "$crate_dir")"
    cp -r "$crate_dir" "$SRC_DIR/$crate_name"
    rm -rf "$SRC_DIR/$crate_name/target" 2>/dev/null || true
done

if [ "$SKIP_VENDOR" != "1" ]; then
    echo "Vendoring Cargo dependencies for offline macOS build..."
    mkdir -p "$SRC_DIR/.cargo"
    (cd "$SRC_DIR" && cargo vendor vendor > .cargo/config.toml)
else
    echo "Skipping cargo vendor (SKIP_VENDOR=1)"
fi

echo "$CONTROLLER_URL" > "$MEDIA_DIR/ozma/controller.txt"

cat > "$MEDIA_DIR/ozma/install-offline.sh" << 'SHEOF'
#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="/tmp/ozma-build"
CONTROLLER_URL="${CONTROLLER_URL:-$(cat "$SCRIPT_DIR/controller.txt" 2>/dev/null || echo http://10.0.2.2:7380)}"

echo "=== Ozma Rust Node (macOS) Self-Bootstrap ==="
if ! command -v cargo >/dev/null 2>&1; then
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        source "$HOME/.cargo/env"
    fi
fi
command -v cargo >/dev/null 2>&1 || { echo "ERROR: cargo not found; install Rust + Xcode CLT"; exit 1; }
echo "Rust: $(rustc --version)"

rm -rf "$BUILD_DIR"
cp -R "$SCRIPT_DIR/src" "$BUILD_DIR"
cd "$BUILD_DIR"

OFFLINE_FLAG=""
if [ -d "$BUILD_DIR/vendor" ]; then
    OFFLINE_FLAG="--offline"
fi

echo "Running cargo test -p ozma-input..."
cargo test $OFFLINE_FLAG -p ozma-input

echo "Building ozma-node (release)..."
cargo build $OFFLINE_FLAG --release -p ozma-node

sudo mkdir -p /usr/local/bin
sudo cp "$BUILD_DIR/target/release/ozma-node" /usr/local/bin/ozma-node
sudo chmod +x /usr/local/bin/ozma-node

PLIST="/Library/LaunchDaemons/com.ozma.node.plist"
sudo tee "$PLIST" >/dev/null << PLISTEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.ozma.node</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/ozma-node</string>
    <string>--profile</string><string>desktop</string>
    <string>--register-url</string><string>${CONTROLLER_URL}</string>
    <string>--register-host</string><string>localhost</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/var/log/ozma-node.log</string>
  <key>StandardErrorPath</key><string>/var/log/ozma-node.err</string>
</dict>
</plist>
PLISTEOF

sudo launchctl unload "$PLIST" 2>/dev/null || true
sudo launchctl load -w "$PLIST"

for _ in $(seq 1 15); do
    if curl -sf http://127.0.0.1:7382/health >/dev/null; then
        echo "Node /health: OK"
        exit 0
    fi
    sleep 1
done
echo "WARNING: /health not responding yet; check /var/log/ozma-node.err"
SHEOF
chmod +x "$MEDIA_DIR/ozma/install-offline.sh"

if command -v genisoimage >/dev/null 2>&1; then
    genisoimage -iso-level 3 -joliet-long -o "$ISO_PATH" -V "OZMA_MACOS" -R -J "$MEDIA_DIR"
    echo "ISO written: $ISO_PATH"
else
    echo "Media staged at $MEDIA_DIR (install genisoimage to pack $ISO_PATH)"
fi
