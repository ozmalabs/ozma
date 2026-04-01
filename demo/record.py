#!/usr/bin/env python3
"""
demo/record.py — Record a demo video of the Ozma Controller web UI.

Opens localhost:7380 in a headless Chromium browser, switches through
scenarios to show RGB transitions and keyboard animations, then saves
a video file.

Usage:
  python demo/record.py [--url http://localhost:7380] [--out demo/recordings/demo.webm]
  python demo/record.py --convert   # also convert webm → mp4 with ffmpeg

Requirements:
  playwright installed via: pipx install playwright && playwright install chromium

The script can optionally start/stop the Ozma demo services itself if they
aren't already running (pass --start-services).
"""

import argparse
import asyncio
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

PIPX_PYTHON = os.environ.get("PLAYWRIGHT_PYTHON", "python3")
REPO_ROOT = Path(__file__).parent.parent


# ---------------------------------------------------------------------------
# Controller interaction

def api(base: str, method: str, path: str, body: dict | None = None) -> dict:
    url = f"{base}{path}"
    data = json.dumps(body).encode() if body else None
    req = urllib.request.Request(url, data=data, method=method)
    if data:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())


def wait_for_api(base: str, timeout: float = 15.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            api(base, "GET", "/api/v1/status")
            return True
        except Exception:
            time.sleep(0.4)
    return False


# ---------------------------------------------------------------------------
# Service management

_procs: list[subprocess.Popen] = []

def start_services(base_url: str) -> None:
    """Start controller + two soft nodes if the API isn't already up."""
    if wait_for_api(base_url, timeout=2.0):
        print("  Controller already running.")
        return

    print("  Starting controller...")
    ctrl = subprocess.Popen(
        [sys.executable,
         str(REPO_ROOT / "controller" / "main.py"),
         "--virtual-only"],
        cwd=str(REPO_ROOT / "controller"),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    _procs.append(ctrl)

    print("  Starting soft nodes...")
    for name, port in [("vm1", 7332), ("vm2", 7333)]:
        proc = subprocess.Popen(
            [sys.executable,
             str(REPO_ROOT / "softnode" / "soft_node.py"),
             "--name", name,
             "--port", str(port),
             "--qmp", f"/tmp/ozma-{name}.qmp"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        _procs.append(proc)

    print("  Waiting for API...", end="", flush=True)
    if not wait_for_api(base_url, timeout=15.0):
        print(" FAILED")
        stop_services()
        sys.exit(1)
    print(" ready.")
    time.sleep(2.0)   # let mDNS discovery complete


def stop_services() -> None:
    for proc in _procs:
        proc.terminate()
    for proc in _procs:
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
    _procs.clear()


# ---------------------------------------------------------------------------
# Recording

def wait_for_streams(base: str, timeout: float = 45.0) -> bool:
    """Wait until at least one HLS stream segment exists on disk."""
    import glob, time
    deadline = time.time() + timeout
    while time.time() < deadline:
        # Check API for active streams
        try:
            data = api(base, "GET", "/api/v1/streams")
            streams = data.get("streams", [])
            active = [s for s in streams if s.get("active")]
            if active:
                print(f"  Stream active: {active[0]['node_id']}")
                time.sleep(3.0)  # let a few HLS segments accumulate
                return True
        except Exception:
            pass
        time.sleep(2.0)
    return False


async def record(base: str, out_dir: Path, video_size: tuple[int, int]) -> Path:
    from playwright.async_api import async_playwright

    width, height = video_size

    async with async_playwright() as p:
        browser = await p.chromium.launch(
            headless=True,
            args=[
                "--enable-webgl",
                "--use-gl=swiftshader",
                "--ignore-gpu-blocklist",
                "--disable-gpu-sandbox",
                "--no-sandbox",
                "--disable-dev-shm-usage",
                "--autoplay-policy=no-user-gesture-required",
                "--disable-backgrounding-occluded-windows",
                "--disable-renderer-backgrounding",
            ],
        )

        context = await browser.new_context(
            viewport={"width": width, "height": height},
            record_video_dir=str(out_dir),
            record_video_size={"width": width, "height": height},
            device_scale_factor=1,
        )
        page = await context.new_page()

        # Navigate and wait for Three.js canvas
        print("  Navigating to", base)
        await page.goto(base, wait_until="domcontentloaded")
        await page.wait_for_selector("canvas", timeout=10000)
        print("  Canvas found — waiting for 3D render + HLS buffering...")
        await asyncio.sleep(8.0)   # let Three.js render + both HLS streams buffer

        # --- Demo sequence ---

        scenarios = api(base, "GET", "/api/v1/scenarios")["scenarios"]
        ids = [s["id"] for s in scenarios]
        names = {s["id"]: s["name"] for s in scenarios}
        colors = {s["id"]: s.get("color", "") for s in scenarios}
        show_ids = ids[:3]

        print(f"  Scenarios: {[names[i] for i in ids]}")

        async def activate(sid: str, wait: float = 3.0) -> None:
            print(f"  → {names[sid]:12s}  {colors[sid]}")
            api(base, "POST", f"/api/v1/scenarios/{sid}/activate")
            await asyncio.sleep(wait)

        async def press_keys(keys: list[str], delay_ms: int = 80) -> None:
            for k in keys:
                await page.keyboard.down(k)
                await asyncio.sleep(delay_ms / 1000)
            await asyncio.sleep(0.12)
            for k in reversed(keys):
                await page.keyboard.up(k)
            await asyncio.sleep(0.2)

        # Two-scenario demo: VM 1 ↔ VM 2 with visible wipe transitions
        a, b = show_ids[0], show_ids[1 % len(show_ids)]

        # Opening: VM 1 (blue keyboard, vm1 stream)
        await activate(a, wait=4.0)
        await press_keys(["KeyV", "KeyM", "Digit1"])

        # Switch to VM 2 — wave_left wipe, green tint
        await activate(b, wait=4.0)
        await press_keys(["KeyV", "KeyM", "Digit2"])

        # Back to VM 1 — wave_right wipe, blue tint
        await activate(a, wait=4.0)

        # Quick back-and-forth to show both transitions clearly
        await activate(b, wait=3.0)
        await activate(a, wait=3.0)
        await activate(b, wait=3.0)
        await activate(a, wait=3.0)

        # Outro hold
        await asyncio.sleep(2.0)

        # Close context — this finalises the video file
        video_path_in_context = await page.video.path()
        await context.close()
        await browser.close()

        return Path(video_path_in_context)


def convert_to_mp4(webm: Path, mp4: Path) -> None:
    print(f"  Converting {webm.name} → {mp4.name}...")
    result = subprocess.run(
        [
            "ffmpeg", "-y",
            "-i", str(webm),
            "-c:v", "libx264",
            "-preset", "fast",
            "-crf", "18",
            "-pix_fmt", "yuv420p",
            "-movflags", "+faststart",
            str(mp4),
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print("  ffmpeg stderr:", result.stderr[-500:], file=sys.stderr)
        raise RuntimeError(f"ffmpeg failed with code {result.returncode}")
    print(f"  Saved: {mp4}")


# ---------------------------------------------------------------------------

def main() -> None:
    p = argparse.ArgumentParser(description="Record Ozma demo video")
    p.add_argument("--url", default="http://localhost:7380")
    p.add_argument("--out", default="demo/recordings/demo.webm",
                   help="Output path (default: demo/recordings/demo.webm)")
    p.add_argument("--size", default="1920x1080",
                   help="Video resolution WxH (default: 1920x1080)")
    p.add_argument("--convert", action="store_true",
                   help="Convert webm output to mp4 via ffmpeg")
    p.add_argument("--start-services", action="store_true",
                   help="Start controller+soft nodes if not already running")
    p.add_argument("--wait-streams", action="store_true",
                   help="Wait for HLS streams to become active before recording")
    args = p.parse_args()

    base = args.url.rstrip("/")
    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    w, h = (int(x) for x in args.size.lower().split("x"))

    if args.start_services:
        print("Starting services...")
        start_services(base)

    # Verify API is reachable
    if not wait_for_api(base, timeout=5.0):
        print(f"ERROR: Controller not reachable at {base}", file=sys.stderr)
        print("  Start it with:  bash demo/run_demo.sh", file=sys.stderr)
        print("  Or pass:        --start-services", file=sys.stderr)
        sys.exit(1)

    status = api(base, "GET", "/api/v1/status")
    node_count = len(status.get("nodes", {}))
    print(f"Controller ready — {node_count} node(s) online")

    if args.wait_streams:
        print("Waiting for HLS streams to become active (VMs must be booting)...")
        if wait_for_streams(base, timeout=90.0):
            print("  Streams ready.")
        else:
            print("  WARNING: No streams became active — monitor will show black screen.")

    # out_dir is the dir; playwright picks the filename
    out_dir = out_path.parent
    print(f"Recording to {out_dir}/ ...")

    try:
        webm_path = asyncio.run(record(base, out_dir, (w, h)))
    finally:
        if args.start_services:
            stop_services()

    # Rename the playwright-generated file to our desired name
    final_webm = out_path.with_suffix(".webm")
    if webm_path != final_webm:
        webm_path.rename(final_webm)
    print(f"  Saved: {final_webm}")

    if args.convert:
        mp4_path = out_path.with_suffix(".mp4")
        convert_to_mp4(final_webm, mp4_path)
    else:
        print("  Tip: pass --convert to also export as mp4")

    print("\nDone.")


if __name__ == "__main__":
    main()
