#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma Doom Agent — AI plays Doom through the ozma stack.

Captures the game screen via ozma's VNC/display capture, feeds frames to
a local VLM (Qwen2.5-VL via Ollama), and sends keyboard actions back
through ozma's HID pipeline. The AI never touches the game directly —
everything goes through ozma, exactly like a human operator would.

Architecture:
  Doom (in VM) ──VNC──► ozma soft node ──HTTP──► this agent
       ▲                                            │
       │                                            ▼
       │                                    Ollama VLM (local)
       │                                            │
       └──── evdev HID ◄── ozma soft node ◄── actions

Usage:
  python3 demo/doom_agent.py                          # auto-detect node
  python3 demo/doom_agent.py --node-api http://localhost:7382
  python3 demo/doom_agent.py --model qwen2.5-vl:3b    # smaller/faster model
  python3 demo/doom_agent.py --controller http://localhost:7380  # use controller API
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import io
import json
import logging
import struct
import sys
import time
from pathlib import Path

log = logging.getLogger("ozma.doom_agent")

# ── Ollama client ──────────────────────────────────────────────────────────

async def ollama_vision(
    image_b64: str,
    prompt: str,
    model: str = "qwen2.5vl:7b",
    api_url: str = "http://localhost:11434",
) -> str:
    """Send an image + prompt to Ollama and get a text response."""
    import aiohttp

    payload = {
        "model": model,
        "prompt": prompt,
        "images": [image_b64],
        "stream": False,
        "options": {
            "temperature": 0.3,
            "num_predict": 150,
        },
    }

    async with aiohttp.ClientSession() as session:
        async with session.post(
            f"{api_url}/api/generate",
            json=payload,
            timeout=aiohttp.ClientTimeout(total=300),
        ) as resp:
            if resp.status != 200:
                text = await resp.text()
                log.warning("Ollama error %d: %s", resp.status, text[:200])
                return ""
            result = await resp.json()
            return result.get("response", "")


# ── Frame capture ──────────────────────────────────────────────────────────

async def capture_frame(node_api: str) -> bytes | None:
    """Capture a JPEG frame from the ozma soft node's display snapshot."""
    import aiohttp

    try:
        async with aiohttp.ClientSession() as session:
            async with session.get(
                f"{node_api}/display/snapshot",
                timeout=aiohttp.ClientTimeout(total=10),
            ) as resp:
                if resp.status == 200 and resp.content_type.startswith("image/"):
                    return await resp.read()
                log.debug("Snapshot returned %d %s", resp.status, resp.content_type)
    except Exception as e:
        log.debug("Frame capture failed: %s", e)
    return None


# ── HID input ──────────────────────────────────────────────────────────────

async def send_key(node_api: str, keycode: int, down: bool) -> None:
    """Send a key press/release to the soft node."""
    import aiohttp

    try:
        async with aiohttp.ClientSession() as session:
            await session.post(
                f"{node_api}/input/key",
                json={"keycode": keycode, "down": down},
                timeout=aiohttp.ClientTimeout(total=2),
            )
    except Exception:
        pass


async def send_keys(node_api: str, keycodes: list[int], duration: float = 0.1) -> None:
    """Press keys for a duration then release."""
    for kc in keycodes:
        await send_key(node_api, kc, True)
    await asyncio.sleep(duration)
    for kc in keycodes:
        await send_key(node_api, kc, False)


async def send_mouse_move(node_api: str, x: int, y: int) -> None:
    """Send a mouse move to the soft node."""
    import aiohttp

    try:
        async with aiohttp.ClientSession() as session:
            await session.post(
                f"{node_api}/input/mouse",
                json={"x": x, "y": y, "action": "move"},
                timeout=aiohttp.ClientTimeout(total=2),
            )
    except Exception:
        pass


# ── Key mappings ───────────────────────────────────────────────────────────

# evdev keycodes for Doom actions
KEYS = {
    "forward":      103,  # KEY_UP
    "backward":     108,  # KEY_DOWN
    "turn_left":    105,  # KEY_LEFT
    "turn_right":   106,  # KEY_RIGHT
    "strafe_left":  51,   # KEY_COMMA (,)
    "strafe_right": 52,   # KEY_DOT (.)
    "shoot":        29,   # KEY_LEFTCTRL
    "use":          57,   # KEY_SPACE
    "run":          54,   # KEY_RIGHTSHIFT
    "enter":        28,   # KEY_ENTER
    "escape":       1,    # KEY_ESC
    "w":            17,   # KEY_W
    "a":            30,   # KEY_A
    "s":            31,   # KEY_S
    "d":            32,   # KEY_D
}


# ── Action parser ──────────────────────────────────────────────────────────

def parse_actions(response: str) -> list[dict]:
    """Parse VLM response into a list of actions.

    Expected format from the VLM:
      ACTIONS: forward, shoot
      ACTIONS: turn_left, forward, shoot
      ACTIONS: use
    """
    actions = []

    # Look for ACTIONS: line
    for line in response.splitlines():
        line = line.strip().upper()
        if line.startswith("ACTIONS:") or line.startswith("ACTION:"):
            parts = line.split(":", 1)[1].strip()
            for action_name in parts.split(","):
                action_name = action_name.strip().lower()
                # Map action names to keycodes
                if action_name in KEYS:
                    actions.append({"type": "key", "name": action_name, "keycode": KEYS[action_name]})
                elif action_name in ("wait", "nothing", "observe"):
                    actions.append({"type": "wait"})

    # Fallback: look for action words anywhere in the response
    if not actions:
        response_lower = response.lower()
        if "shoot" in response_lower or "fire" in response_lower or "attack" in response_lower:
            actions.append({"type": "key", "name": "shoot", "keycode": KEYS["shoot"]})
        if "forward" in response_lower or "move ahead" in response_lower or "go forward" in response_lower:
            actions.append({"type": "key", "name": "forward", "keycode": KEYS["forward"]})
        if "turn left" in response_lower or "look left" in response_lower:
            actions.append({"type": "key", "name": "turn_left", "keycode": KEYS["turn_left"]})
        if "turn right" in response_lower or "look right" in response_lower:
            actions.append({"type": "key", "name": "turn_right", "keycode": KEYS["turn_right"]})
        if "backward" in response_lower or "back up" in response_lower or "retreat" in response_lower:
            actions.append({"type": "key", "name": "backward", "keycode": KEYS["backward"]})
        if "open" in response_lower or "use" in response_lower or "door" in response_lower:
            actions.append({"type": "key", "name": "use", "keycode": KEYS["use"]})
        if "strafe left" in response_lower or "dodge left" in response_lower:
            actions.append({"type": "key", "name": "strafe_left", "keycode": KEYS["strafe_left"]})
        if "strafe right" in response_lower or "dodge right" in response_lower:
            actions.append({"type": "key", "name": "strafe_right", "keycode": KEYS["strafe_right"]})

    return actions


# ── Game prompt ────────────────────────────────────────────────────────────

DOOM_PROMPT = """You are an AI agent playing the classic FPS game DOOM. You see a screenshot of the game.

Analyze the screenshot and decide what actions to take. Available actions:
- forward: move forward
- backward: move backward / retreat
- turn_left: turn/look left
- turn_right: turn/look right
- strafe_left: sidestep left (dodge)
- strafe_right: sidestep right (dodge)
- shoot: fire your weapon (use when enemies are visible and roughly centered)
- use: interact/open doors (use when facing a door or switch)
- wait: do nothing this frame

Rules:
1. If you see an enemy (monster/demon), turn to face it and shoot
2. If you see a door, walk toward it and use it
3. If you see a corridor or room, move forward to explore
4. If you're taking damage (screen flashing red), strafe to dodge
5. If you see items (health, ammo, armor), move toward them
6. Keep moving — standing still gets you killed

Respond with EXACTLY one line starting with "ACTIONS:" followed by comma-separated actions.
Example: ACTIONS: forward, shoot
Example: ACTIONS: turn_left, forward
Example: ACTIONS: strafe_right, shoot

What actions should you take?"""


# ── Main loop ──────────────────────────────────────────────────────────────

async def doom_loop(
    node_api: str,
    model: str = "qwen2.5vl:7b",
    ollama_url: str = "http://localhost:11434",
    fps_target: float = 2.0,
    max_steps: int = 0,
) -> None:
    """Main agent loop: capture → think → act → repeat."""
    log.info("Doom Agent starting")
    log.info("  Node API:  %s", node_api)
    log.info("  Model:     %s", model)
    log.info("  Ollama:    %s", ollama_url)
    log.info("  Target:    %.1f decisions/sec", fps_target)

    step = 0
    total_think_time = 0
    total_act_time = 0
    kills = 0  # we can't really count these but it's fun

    frame_interval = 1.0 / fps_target

    while max_steps == 0 or step < max_steps:
        step += 1
        loop_start = time.time()

        # 1. Capture frame
        frame_data = await capture_frame(node_api)
        if not frame_data:
            log.warning("No frame — retrying in 1s")
            await asyncio.sleep(1)
            continue

        frame_b64 = base64.b64encode(frame_data).decode()
        frame_size = len(frame_data) / 1024

        # 2. Send to VLM
        t0 = time.time()
        response = await ollama_vision(frame_b64, DOOM_PROMPT, model, ollama_url)
        think_time = time.time() - t0
        total_think_time += think_time

        if not response:
            log.warning("Empty VLM response — skipping")
            await asyncio.sleep(0.5)
            continue

        # 3. Parse actions
        actions = parse_actions(response)
        action_names = [a.get("name", "wait") for a in actions]

        # 4. Execute actions
        t0 = time.time()
        key_actions = [a for a in actions if a["type"] == "key"]
        if key_actions:
            keycodes = [a["keycode"] for a in key_actions]
            await send_keys(node_api, keycodes, duration=0.15)
        act_time = time.time() - t0
        total_act_time += act_time

        # 5. Log
        total_time = time.time() - loop_start
        avg_think = total_think_time / step
        log.info(
            "[%04d] %s  think=%.0fms act=%.0fms total=%.0fms frame=%.0fKB",
            step, ",".join(action_names) or "wait",
            think_time * 1000, act_time * 1000, total_time * 1000, frame_size,
        )

        # 6. Rate limit
        elapsed = time.time() - loop_start
        if elapsed < frame_interval:
            await asyncio.sleep(frame_interval - elapsed)

    avg_think = total_think_time / max(step, 1)
    log.info("Agent stopped after %d steps. Avg think time: %.0fms", step, avg_think * 1000)


# ── CLI ────────────────────────────────────────────────────────────────────

def main() -> None:
    p = argparse.ArgumentParser(
        description="Ozma Doom Agent — AI plays Doom through the ozma stack",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
The AI captures game frames from an ozma node, sends them to a local
vision model (Ollama), and injects keyboard actions back through ozma.

The full pipeline: Doom (VM) → VNC → ozma node → agent → VLM → agent → ozma HID → VM

Examples:
  %(prog)s                                           # auto-detect everything
  %(prog)s --node-api http://localhost:7382           # explicit node
  %(prog)s --model qwen2.5-vl:3b                     # smaller model
  %(prog)s --fps 1                                   # slower, more deliberate
""")
    p.add_argument("--node-api", default="http://localhost:7382",
                   help="Soft node HTTP API URL (default: http://localhost:7382)")
    p.add_argument("--model", default="qwen2.5vl:7b",
                   help="Ollama vision model (default: qwen2.5vl:7b)")
    p.add_argument("--ollama", default="http://localhost:11434",
                   help="Ollama API URL (default: http://localhost:11434)")
    p.add_argument("--fps", type=float, default=2.0,
                   help="Target decisions per second (default: 2)")
    p.add_argument("--steps", type=int, default=0,
                   help="Max steps (0 = unlimited)")
    p.add_argument("--debug", action="store_true")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    asyncio.run(doom_loop(
        node_api=args.node_api,
        model=args.model,
        ollama_url=args.ollama,
        fps_target=args.fps,
        max_steps=args.steps,
    ))


if __name__ == "__main__":
    main()
