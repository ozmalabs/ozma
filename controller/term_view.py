#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
term_view — render an Ozma node's display as ANSI art in your terminal.

Usage:
  python3 controller/term_view.py <node_id> [options]

Examples:
  python3 controller/term_view.py vm1
  python3 controller/term_view.py vm1 --stream --fps 15
  python3 controller/term_view.py vm1 --mode ocr --cols 132 --rows 50
  python3 controller/term_view.py vm1 --pixel-mode sixel

The controller must be running at the URL given by --url (default
http://localhost:7380).  If auth is enabled, provide --token or set
the OZMA_TOKEN environment variable.
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import urllib.request
import urllib.error
from typing import Iterator


def _terminal_size() -> tuple[int, int]:
    """Return (cols, rows) of the calling terminal, or (80, 24) as fallback."""
    try:
        ts = shutil.get_terminal_size()
        # Reserve one row so the shell prompt doesn't overwrite the last line
        return ts.columns, max(ts.lines - 1, 1)
    except Exception:
        return 80, 24


def _build_url(base: str, node_id: str, args: argparse.Namespace) -> str:
    cols, rows = _terminal_size()
    if args.cols:
        cols = args.cols
    if args.rows:
        rows = args.rows

    params = [
        f"mode={args.mode}",
        f"cols={cols}",
        f"rows={rows}",
        f"pixel_mode={args.pixel_mode}",
    ]
    if args.stream:
        params.append(f"stream=1")
        params.append(f"fps={args.fps}")

    base = base.rstrip("/")
    return f"{base}/api/v1/remote/{node_id}/view?{'&'.join(params)}"


def _request(url: str, token: str | None) -> urllib.request.Request:
    req = urllib.request.Request(url)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    return req


def _snapshot(url: str, token: str | None) -> None:
    """Fetch a single frame and write it to stdout."""
    try:
        with urllib.request.urlopen(_request(url, token), timeout=10) as resp:
            data = resp.read()
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()
    except urllib.error.HTTPError as e:
        print(f"HTTP {e.code}: {e.reason}", file=sys.stderr)
        sys.exit(1)
    except urllib.error.URLError as e:
        print(f"Connection error: {e.reason}", file=sys.stderr)
        sys.exit(1)


def _stream(url: str, token: str | None) -> None:
    """Stream frames until interrupted."""
    try:
        with urllib.request.urlopen(_request(url, token), timeout=60) as resp:
            while True:
                chunk = resp.read(65536)
                if not chunk:
                    break
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
    except KeyboardInterrupt:
        # Clean up terminal state on Ctrl-C
        sys.stdout.buffer.write(b"\x1b[0m\x1b[2J\x1b[H")
        sys.stdout.buffer.flush()
    except urllib.error.HTTPError as e:
        print(f"HTTP {e.code}: {e.reason}", file=sys.stderr)
        sys.exit(1)
    except urllib.error.URLError as e:
        print(f"Connection error: {e.reason}", file=sys.stderr)
        sys.exit(1)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Render an Ozma node's display as ANSI art in your terminal",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("node_id", help="Node ID (e.g. vm1, desk-left)")
    parser.add_argument("--url", default=os.environ.get("OZMA_URL", "http://localhost:7380"),
                        help="Controller base URL (env: OZMA_URL, default: http://localhost:7380)")
    parser.add_argument("--token", default=os.environ.get("OZMA_TOKEN"),
                        help="Bearer token for auth-enabled controllers (env: OZMA_TOKEN)")
    parser.add_argument("--stream", "-s", action="store_true",
                        help="Stream frames continuously instead of a single snapshot")
    parser.add_argument("--fps", type=float, default=10.0,
                        help="Target frames per second for --stream (default: 10)")
    parser.add_argument("--mode", choices=["auto", "ocr", "pixel"], default="auto",
                        help="Rendering mode: auto (OCR if text, else pixel), ocr, pixel (default: auto)")
    parser.add_argument("--cols", type=int, default=0,
                        help="Terminal columns (default: detect from $COLUMNS / tty)")
    parser.add_argument("--rows", type=int, default=0,
                        help="Terminal rows (default: detect from $LINES / tty)")
    parser.add_argument("--pixel-mode", dest="pixel_mode",
                        choices=["auto", "sixel", "kitty", "half", "braille"], default="auto",
                        help="Pixel rendering backend hint (default: auto)")
    args = parser.parse_args()

    url = _build_url(args.url, args.node_id, args)

    if args.stream:
        _stream(url, args.token)
    else:
        _snapshot(url, args.token)


if __name__ == "__main__":
    main()
