# Ozma WebRTC helper

`gst_webrtc_worker.py` is the external GStreamer WebRTC worker that
`ozma-node` supervises when `--webrtc-helper` is configured. It is Python
(not Rust) because it depends on the system GStreamer bindings (`gi`,
`webrtcbin`, `rtph264pay`, `x264enc` or `nvh264enc`). It lives here, outside
`softnode/`, so the Rust node no longer depends on the Python node tree path.

## Run

```bash
# The node spawns this program with the script as the first argument.
ozma-node --profile vm \
    --webrtc-helper python3 \
    --webrtc-helper-script "$PWD/scripts/webrtc/gst_webrtc_worker.py"
```

Or set the equivalent environment variables:

```bash
export OZMA_WEBRTC_HELPER=python3
export OZMA_WEBRTC_HELPER_SCRIPT="$PWD/scripts/webrtc/gst_webrtc_worker.py"
```

`scripts/install-rust.sh` installs the helper to
`<prefix>/share/ozma/webrtc/gst_webrtc_worker.py` (for example
`~/.local/share/ozma/webrtc/` or `/usr/local/share/ozma/webrtc/`) so a packaged
node does not need a repository checkout. Pass the installed path through
`--webrtc-helper-script` or `OZMA_WEBRTC_HELPER_SCRIPT`.

## Compatibility shim

`softnode/gst_webrtc_worker.py` remains as a one-release shim. It re-exports
the canonical module for `import gst_webrtc_worker` (used by
`softnode/gst_webrtc_worker_test.py` and `softnode/gst_webrtc.py`) and executes
the canonical file when run as a script. Lookup order is
`$OZMA_WEBRTC_WORKER`, the repo copy, `<prefix>/share/ozma/webrtc/`, and
`~/.local/share/ozma/webrtc/`.

## Test

```bash
python3 softnode/gst_webrtc_worker_test.py
python3 -m py_compile scripts/webrtc/gst_webrtc_worker.py
```

The live helper path needs a GStreamer host; see
[`docs/gate2-validation.md`](../../docs/gate2-validation.md) and
[`docs/phase4-install-cutover.md`](../../docs/phase4-install-cutover.md).
