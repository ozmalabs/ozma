#!/usr/bin/env python3
"""
E2E tests for V0.5 API endpoints — audio nodes, volume/mute, outputs,
controls.

Requirements:
  - Controller running on localhost:7380
  - PipeWire running (for audio endpoint tests)

Usage:
  python tests/test_e2e_v05.py [--host localhost] [--port 7380]
"""

import argparse
import json
import shutil
import sys
import time
import urllib.error
import urllib.request

BASE_URL = "http://localhost:7380"
TIMEOUT = 5.0


def api_get(path: str) -> dict:
    url = f"{BASE_URL}{path}"
    with urllib.request.urlopen(url, timeout=TIMEOUT) as r:
        return json.loads(r.read())


def api_post(path: str, body: dict | None = None) -> dict:
    url = f"{BASE_URL}{path}"
    data = json.dumps(body or {}).encode()
    req = urllib.request.Request(
        url, data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
        return json.loads(r.read())


def check(condition: bool, msg: str) -> None:
    if condition:
        print(f"  PASS  {msg}")
    else:
        print(f"  FAIL  {msg}")
        sys.exit(1)


def skip(msg: str) -> None:
    print(f"  SKIP  {msg}")


# ── Audio node endpoints ─────────────────────────────────────────────────────

def test_audio_nodes() -> None:
    print("\n[V0.5-1] GET /audio/nodes — PipeWire node state")
    if not shutil.which("pw-dump"):
        skip("pw-dump not found")
        return
    data = api_get("/api/v1/audio/nodes")
    check("nodes" in data, "response has 'nodes' key")
    check("links" in data, "response has 'links' key")
    check(isinstance(data["nodes"], dict), "nodes is a dict")
    check(isinstance(data["links"], list), "links is a list")

    if data["nodes"]:
        first_name = list(data["nodes"].keys())[0]
        node = data["nodes"][first_name]
        check("volume" in node, f"node '{first_name}' has volume")
        check("mute" in node, f"node '{first_name}' has mute")
        check("media_class" in node, f"node '{first_name}' has media_class")
        print(f"  INFO  Found {len(data['nodes'])} PW audio nodes, {len(data['links'])} links")
    else:
        print("  INFO  No PW audio nodes (PipeWire may not be running)")


def test_audio_links() -> None:
    print("\n[V0.5-2] GET /audio/links — PipeWire link state")
    data = api_get("/api/v1/audio/links")
    check("links" in data, "response has 'links' key")
    print(f"  INFO  {len(data['links'])} audio links")


# ── Volume / mute control ────────────────────────────────────────────────────

def test_volume_control() -> None:
    print("\n[V0.5-3] POST /audio/volume — set PipeWire node volume")
    # Get a node to test with
    data = api_get("/api/v1/audio/nodes")
    if not data.get("nodes"):
        skip("No PW nodes available")
        return

    node_name = list(data["nodes"].keys())[0]
    original_vol = data["nodes"][node_name]["volume"]

    # Set volume
    try:
        result = api_post("/api/v1/audio/volume", {
            "node_name": node_name, "volume": 0.42,
        })
        check(result.get("ok") is True, f"set volume on '{node_name}' to 0.42")
    except urllib.error.HTTPError as e:
        check(False, f"set volume failed: {e}")
        return

    # Verify it changed
    time.sleep(1.5)  # wait for PW watcher poll
    data2 = api_get("/api/v1/audio/nodes")
    new_vol = data2["nodes"].get(node_name, {}).get("volume", -1)
    check(abs(new_vol - 0.42) < 0.05, f"volume reads back ~0.42 (got {new_vol:.3f})")

    # Restore original
    api_post("/api/v1/audio/volume", {"node_name": node_name, "volume": original_vol})
    print(f"  INFO  Restored volume to {original_vol:.3f}")


def test_mute_control() -> None:
    print("\n[V0.5-4] POST /audio/mute — set PipeWire node mute")
    data = api_get("/api/v1/audio/nodes")
    if not data.get("nodes"):
        skip("No PW nodes available")
        return

    node_name = list(data["nodes"].keys())[0]
    original_mute = data["nodes"][node_name]["mute"]

    try:
        result = api_post("/api/v1/audio/mute", {
            "node_name": node_name, "mute": True,
        })
        check(result.get("ok") is True, f"set mute on '{node_name}'")
    except urllib.error.HTTPError as e:
        check(False, f"set mute failed: {e}")
        return

    time.sleep(1.5)
    data2 = api_get("/api/v1/audio/nodes")
    new_mute = data2["nodes"].get(node_name, {}).get("mute")
    check(new_mute is True, f"mute reads back True (got {new_mute})")

    # Restore
    api_post("/api/v1/audio/mute", {"node_name": node_name, "mute": original_mute})


def test_volume_nonexistent_node() -> None:
    print("\n[V0.5-5] POST /audio/volume on nonexistent node → 404")
    try:
        api_post("/api/v1/audio/volume", {
            "node_name": "nonexistent-node-xyz", "volume": 0.5,
        })
        check(False, "should have returned 404")
    except urllib.error.HTTPError as e:
        check(e.code == 404, f"got 404 (status={e.code})")


# ── Audio outputs ────────────────────────────────────────────────────────────

def test_audio_outputs() -> None:
    print("\n[V0.5-6] GET /audio/outputs — list audio output targets")
    data = api_get("/api/v1/audio/outputs")
    check("outputs" in data, "response has 'outputs' key")
    check(len(data["outputs"]) >= 1, "at least 1 output (local)")

    local = next((o for o in data["outputs"] if o["id"] == "local"), None)
    check(local is not None, "local output exists")
    check(local["enabled"] is True, "local output is enabled by default")
    check("delay_ms" in local, "output has delay_ms field")

    print(f"  INFO  {len(data['outputs'])} output(s): " +
          ", ".join(f"{o['id']} ({o['protocol']})" for o in data["outputs"]))


def test_output_enable_disable() -> None:
    print("\n[V0.5-7] Enable/disable audio outputs")
    # Disable local
    result = api_post("/api/v1/audio/outputs/disable", {"output_id": "local"})
    check(result.get("ok") is True, "disable local output")

    data = api_get("/api/v1/audio/outputs")
    local = next(o for o in data["outputs"] if o["id"] == "local")
    check(local["enabled"] is False, "local output now disabled")

    # Re-enable
    result = api_post("/api/v1/audio/outputs/enable", {"output_id": "local"})
    check(result.get("ok") is True, "re-enable local output")

    data = api_get("/api/v1/audio/outputs")
    local = next(o for o in data["outputs"] if o["id"] == "local")
    check(local["enabled"] is True, "local output now enabled")


def test_output_delay() -> None:
    print("\n[V0.5-8] Set audio output delay")
    result = api_post("/api/v1/audio/outputs/delay", {
        "output_id": "local", "delay_ms": 150.0,
    })
    check(result.get("ok") is True, "set delay to 150ms")

    data = api_get("/api/v1/audio/outputs")
    local = next(o for o in data["outputs"] if o["id"] == "local")
    check(abs(local["delay_ms"] - 150.0) < 1.0, f"delay reads back ~150ms (got {local['delay_ms']})")

    # Reset to 0
    api_post("/api/v1/audio/outputs/delay", {"output_id": "local", "delay_ms": 0})


def test_output_nonexistent() -> None:
    print("\n[V0.5-9] Enable nonexistent output → 404")
    try:
        api_post("/api/v1/audio/outputs/enable", {"output_id": "nonexistent-xyz"})
        check(False, "should have returned 404")
    except urllib.error.HTTPError as e:
        check(e.code == 404, f"got 404 (status={e.code})")


# ── Control surfaces ─────────────────────────────────────────────────────────

def test_controls_endpoint() -> None:
    print("\n[V0.5-10] GET /controls — list control surfaces")
    data = api_get("/api/v1/controls")
    check("surfaces" in data, "response has 'surfaces' key")
    check(len(data["surfaces"]) >= 1, "at least 1 surface (hotkeys)")

    hotkeys = next((s for s in data["surfaces"] if s["id"] == "hotkeys"), None)
    check(hotkeys is not None, "hotkeys surface exists")
    check("next_scenario" in hotkeys["controls"], "hotkeys has next_scenario control")
    check("prev_scenario" in hotkeys["controls"], "hotkeys has prev_scenario control")

    print(f"  INFO  {len(data['surfaces'])} surface(s): " +
          ", ".join(s["id"] for s in data["surfaces"]))


# ── Status endpoint includes new data ────────────────────────────────────────

def test_status_has_scenario_id() -> None:
    print("\n[V0.5-11] GET /status includes active_scenario_id")
    data = api_get("/api/v1/status")
    check("active_scenario_id" in data, "status has active_scenario_id")
    check("nodes" in data, "status has nodes")
    check("active_node_id" in data, "status has active_node_id")


# ── Main ─────────────────────────────────────────────────────────────────────

def main() -> None:
    p = argparse.ArgumentParser(description="Ozma V0.5 E2E tests")
    p.add_argument("--host", default="localhost")
    p.add_argument("--port", type=int, default=7380)
    args = p.parse_args()

    global BASE_URL
    BASE_URL = f"http://{args.host}:{args.port}"

    print(f"Ozma V0.5 E2E Tests — controller at {BASE_URL}")
    print("=" * 60)

    try:
        api_get("/api/v1/status")
    except urllib.error.URLError as e:
        print(f"ERROR: Controller not reachable at {BASE_URL}: {e}")
        print("  Start with: python controller/main.py")
        sys.exit(1)

    test_audio_nodes()
    test_audio_links()
    test_volume_control()
    test_mute_control()
    test_volume_nonexistent_node()
    test_audio_outputs()
    test_output_enable_disable()
    test_output_delay()
    test_output_nonexistent()
    test_controls_endpoint()
    test_status_has_scenario_id()

    print("\n" + "=" * 60)
    print("All V0.5 tests passed.")


if __name__ == "__main__":
    main()
