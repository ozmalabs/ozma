#!/usr/bin/env python3
"""
demo/switch.py — Automated scenario-switching demo.

Repeatedly switches between VM 1 and VM 2 scenarios with a configurable
dwell time, printing a live status line and WebSocket events to stdout.

Usage:
  python demo/switch.py [--url http://localhost:7380] [--dwell 3.0] [--count 6]

Options:
  --url     Controller base URL (default: http://localhost:7380)
  --dwell   Seconds to stay on each scenario before switching (default: 3.0)
  --count   Total number of switches (default: indefinite — Ctrl-C to stop)
  --once    Switch once then exit (useful for manual step-through)
"""

import argparse
import asyncio
import json
import sys
import urllib.error
import urllib.request
from typing import Any


def activate(base_url: str, scenario_id: str) -> dict[str, Any]:
    url = f"{base_url}/api/v1/scenarios/{scenario_id}/activate"
    req = urllib.request.Request(url, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        raise RuntimeError(f"HTTP {e.code}: {body}") from e


def get_scenarios(base_url: str) -> list[dict]:
    url = f"{base_url}/api/v1/scenarios"
    with urllib.request.urlopen(url, timeout=5) as r:
        data = json.loads(r.read())
    return data.get("scenarios", [])


def get_status(base_url: str) -> dict[str, Any]:
    with urllib.request.urlopen(f"{base_url}/api/v1/status", timeout=5) as r:
        return json.loads(r.read())


def main() -> None:
    p = argparse.ArgumentParser(description="Ozma scenario switch demo")
    p.add_argument("--url",   default="http://localhost:7380")
    p.add_argument("--dwell", type=float, default=3.0,
                   help="Seconds on each scenario (default 3.0)")
    p.add_argument("--count", type=int, default=0,
                   help="Number of switches, 0 = infinite (default 0)")
    p.add_argument("--once", action="store_true",
                   help="Switch once and exit")
    args = p.parse_args()

    base = args.url.rstrip("/")

    # Verify controller is reachable
    try:
        status = get_status(base)
    except Exception as e:
        print(f"ERROR: Cannot reach controller at {base}: {e}", file=sys.stderr)
        sys.exit(1)

    scenarios = get_scenarios(base)
    if len(scenarios) < 2:
        print(f"ERROR: Need at least 2 scenarios, found {len(scenarios)}", file=sys.stderr)
        sys.exit(1)

    ids = [s["id"] for s in scenarios]
    names = {s["id"]: s["name"] for s in scenarios}
    colors = {s["id"]: s.get("color", "") for s in scenarios}

    print(f"Ozma switch demo — {base}")
    print(f"Scenarios: {', '.join(f'{names[i]} ({i})' for i in ids)}")
    print(f"Dwell: {args.dwell}s per scenario")
    print()

    current_idx = 0
    switches = 0

    # Start with the first scenario
    current_id = ids[current_idx]
    try:
        result = activate(base, current_id)
        print(f"  → Activated: {names[current_id]:12s}  id={current_id}  color={colors[current_id]}")
    except RuntimeError as e:
        print(f"  ERROR activating {current_id}: {e}", file=sys.stderr)

    if args.once:
        return

    try:
        while True:
            import time
            time.sleep(args.dwell)

            # Advance to next scenario (cycle)
            current_idx = (current_idx + 1) % len(ids)
            current_id = ids[current_idx]
            switches += 1

            try:
                activate(base, current_id)
                print(f"  → Activated: {names[current_id]:12s}  id={current_id}  color={colors[current_id]}  "
                      f"(switch #{switches})")
            except RuntimeError as e:
                print(f"  ERROR activating {current_id}: {e}", file=sys.stderr)

            if args.count and switches >= args.count:
                break

    except KeyboardInterrupt:
        print(f"\nStopped after {switches} switches.")


if __name__ == "__main__":
    main()
