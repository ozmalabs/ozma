#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Integration test: Virtual Node + Windows Agent.

Exercises the complete pipeline:
  - QEMU VM with input-linux evdev
  - VNC screen capture + OmniParser + Tesseract OCR
  - AI agent engine (screenshot, read_screen, type, click, assert)
  - Windows agent (start, verify, stop)
  - Multi-window RPA (Notepad, Calculator, File Explorer)

Prerequisites:
  - QEMU VM running with evdev input-linux devices
  - Windows 10 desktop visible
  - ozma-agent.exe built at C:\\ozma-agent\\dist\\
  - evdev keyboard at /dev/input/event4, mouse at /dev/input/event5
    (or set OZMA_EVDEV_KBD / OZMA_EVDEV_MOUSE env vars)

Usage:
  python3 tests/test_virtual_node_agent.py
  python3 tests/test_virtual_node_agent.py --quick     # skip slow tests
  python3 tests/test_virtual_node_agent.py --verbose    # show all OCR text
"""

from __future__ import annotations

import asyncio
import base64
import json
import os
import sys
import time
from pathlib import Path

# Add controller to path
sys.path.insert(0, str(Path(__file__).parent.parent / "controller"))

from agent_engine import AgentEngine, ActionResult
from screen_reader import ScreenReader

# Optional: OmniParser
try:
    from vision_providers import VisionProviderManager, OmniParserProvider
    _OMNI = True
except ImportError:
    _OMNI = False


# ── Config ────────────────────────────────────────────────────────────────

VNC_HOST = os.environ.get("OZMA_VNC_HOST", "127.0.0.1")
VNC_PORT = int(os.environ.get("OZMA_VNC_PORT", "5931"))
EVDEV_KBD = os.environ.get("OZMA_EVDEV_KBD", "/dev/input/event4")
EVDEV_MOUSE = os.environ.get("OZMA_EVDEV_MOUSE", "/dev/input/event5")
NODE_ID = "win10._ozma._udp.local."
RESULTS_DIR = Path(__file__).parent.parent / "test_results" / "virtual_node_agent"


# ── Test harness ──────────────────────────────────────────────────────────

class VirtualNodeTestResult:
    def __init__(self, name: str):
        self.name = name
        self.passed = False
        self.error = ""
        self.duration = 0.0
        self.screenshot = ""

    def __repr__(self):
        status = "PASS" if self.passed else "FAIL"
        return f"[{status}] {self.name} ({self.duration:.1f}s){' — ' + self.error if self.error else ''}"


class IntegrationTest:
    def __init__(self, quick: bool = False, verbose: bool = False):
        self.quick = quick
        self.verbose = verbose
        self.results: list[VirtualNodeTestResult] = []
        self.engine: AgentEngine | None = None

    async def setup(self):
        RESULTS_DIR.mkdir(parents=True, exist_ok=True)

        # Create screen reader with OmniParser if available
        if _OMNI:
            omni = OmniParserProvider()
            mgr = VisionProviderManager()
            mgr.add(omni)
            reader = ScreenReader(vision_manager=mgr)
        else:
            reader = ScreenReader()

        class Node:
            id = NODE_ID
            host = "127.0.0.1"
            port = 7340
            vnc_host = VNC_HOST
            vnc_port = VNC_PORT
            api_port = 7390

        class State:
            nodes = {NODE_ID: Node()}
            active_node_id = NODE_ID
            def get_active_node(self):
                return self.nodes.get(self.active_node_id)

        self.engine = AgentEngine(
            State(), reader,
            evdev_kbd_path=EVDEV_KBD,
            evdev_mouse_path=EVDEV_MOUSE,
        )

        # Warm up models
        await self.engine.execute("screenshot", node_id=NODE_ID)

    async def run_test(self, name: str, coro) -> VirtualNodeTestResult:
        result = VirtualNodeTestResult(name)
        t0 = time.time()
        try:
            await coro(result)
            if not result.error:
                result.passed = True
        except Exception as e:
            result.error = str(e)
        result.duration = time.time() - t0

        # Save screenshot on failure
        if not result.passed:
            r = await self.engine.execute("screenshot", node_id=NODE_ID)
            if r.screenshot_base64:
                path = RESULTS_DIR / f"FAIL_{name.replace(' ', '_')}.jpg"
                path.write_bytes(base64.b64decode(r.screenshot_base64))
                result.screenshot = str(path)

        self.results.append(result)
        print(result)
        return result

    async def screenshot(self, name: str):
        r = await self.engine.execute("screenshot", node_id=NODE_ID)
        if r.screenshot_base64:
            path = RESULTS_DIR / f"{name.replace(' ', '_')}.jpg"
            path.write_bytes(base64.b64decode(r.screenshot_base64))

    async def type_cmd(self, text: str):
        await self.engine.execute("type", node_id=NODE_ID, text=text)
        await asyncio.sleep(0.2)
        await self.engine.execute("key", node_id=NODE_ID, key="enter")

    async def read_text(self) -> str:
        r = await self.engine.execute("read_screen", node_id=NODE_ID)
        if self.verbose:
            print(f"    OCR: {r.screen_text[:200]}")
        return r.screen_text

    async def assert_text(self, text: str) -> bool:
        r = await self.engine.execute("assert_text", node_id=NODE_ID, text=text)
        return r.success

    # ── Tests ──────────────────────────────────────────────────────────

    async def run_all(self):
        await self.setup()
        t0 = time.time()

        print(f"\n{'='*60}")
        print(f"Ozma Integration Test: Virtual Node + Windows Agent")
        print(f"{'='*60}\n")

        # Phase 1: VM + Input
        print("── Phase 1: VM + Input ──")
        await self.run_test("Desktop visible", self.test_desktop_visible)
        await self.run_test("Keyboard input works", self.test_keyboard_input)
        await self.run_test("Mouse input works", self.test_mouse_input)

        # Phase 2: Python + Agent
        print("\n── Phase 2: Python + Agent ──")
        await self.run_test("Python installed", self.test_python_installed)
        await self.run_test("Agent exe exists", self.test_agent_exists)
        await self.run_test("Agent help", self.test_agent_help)

        # Phase 3: Screen understanding
        print("\n── Phase 3: Screen Understanding ──")
        await self.run_test("OCR reads text", self.test_ocr)
        await self.run_test("Element detection", self.test_elements)
        await self.run_test("SoM overlay", self.test_som)

        # Phase 4: RPA
        print("\n── Phase 4: RPA ──")
        await self.run_test("Open Notepad", self.test_open_notepad)
        await self.run_test("Type in Notepad", self.test_type_notepad)
        await self.run_test("Close Notepad", self.test_close_notepad)

        if not self.quick:
            # Phase 5: Agent runtime
            print("\n── Phase 5: Agent Runtime ──")
            await self.run_test("Start agent", self.test_start_agent)
            await self.run_test("Agent running", self.test_agent_running)
            await self.run_test("Stop agent", self.test_stop_agent)

        # Summary
        elapsed = time.time() - t0
        passed = sum(1 for r in self.results if r.passed)
        failed = sum(1 for r in self.results if not r.passed)

        print(f"\n{'='*60}")
        print(f"Results: {passed} passed, {failed} failed, {len(self.results)} total")
        print(f"Time: {elapsed:.0f}s")
        print(f"{'='*60}")

        if failed:
            print("\nFailed tests:")
            for r in self.results:
                if not r.passed:
                    print(f"  {r}")

        return failed == 0

    # ── Phase 1: VM + Input ───────────────────────────────────────────

    async def test_desktop_visible(self, result: VirtualNodeTestResult):
        r = await self.engine.execute("screenshot", node_id=NODE_ID)
        if not r.success or not r.screenshot_base64:
            result.error = "Cannot capture screenshot"
            return
        # Check it's not black
        import base64
        data = base64.b64decode(r.screenshot_base64)
        if len(data) < 1000:
            result.error = "Screenshot too small (black screen?)"

    async def test_keyboard_input(self, result: VirtualNodeTestResult):
        # Open cmd
        await self.engine.execute("hotkey", node_id=NODE_ID, keys=["win", "r"])
        await asyncio.sleep(1.5)
        await self.engine.execute("type", node_id=NODE_ID, text="cmd")
        await asyncio.sleep(0.3)
        await self.engine.execute("key", node_id=NODE_ID, key="enter")
        await asyncio.sleep(2)

        await self.type_cmd("echo KB_TEST_OK")
        await asyncio.sleep(1)
        if not await self.assert_text("KB_TEST_OK"):
            result.error = "Typed text not found on screen"

    async def test_mouse_input(self, result: VirtualNodeTestResult):
        # Click somewhere on the desktop
        r = await self.engine.execute("click", node_id=NODE_ID, x=500, y=400)
        if not r.success:
            result.error = "Mouse click failed"

    # ── Phase 2: Python + Agent ───────────────────────────────────────

    async def test_python_installed(self, result: VirtualNodeTestResult):
        await self.type_cmd("python --version")
        await asyncio.sleep(2)
        if not await self.assert_text("3.13"):
            result.error = "Python 3.13 not found"

    async def test_agent_exists(self, result: VirtualNodeTestResult):
        await self.type_cmd("dir C:\\ozma-agent\\dist\\ozma-agent.exe")
        await asyncio.sleep(2)
        text = await self.read_text()
        if "ozma-agent" not in text.lower() or "not found" in text.lower():
            result.error = "ozma-agent.exe not found"

    async def test_agent_help(self, result: VirtualNodeTestResult):
        await self.type_cmd("C:\\ozma-agent\\dist\\ozma-agent.exe --help")
        await asyncio.sleep(3)
        if not await self.assert_text("make any machine part"):
            result.error = "Agent help text not found"
        await self.screenshot("agent_help")

    # ── Phase 3: Screen Understanding ─────────────────────────────────

    async def test_ocr(self, result: VirtualNodeTestResult):
        text = await self.read_text()
        if len(text) < 20:
            result.error = f"OCR returned too little text ({len(text)} chars)"

    async def test_elements(self, result: VirtualNodeTestResult):
        r = await self.engine.execute("find_elements", node_id=NODE_ID, som=True)
        if len(r.som_elements) < 5:
            result.error = f"Only {len(r.som_elements)} elements found (expected >5)"

    async def test_som(self, result: VirtualNodeTestResult):
        r = await self.engine.execute("screenshot", node_id=NODE_ID, som=True)
        if not r.som_elements:
            result.error = "No SoM elements generated"
        await self.screenshot("som_overlay")

    # ── Phase 4: RPA ──────────────────────────────────────────────────

    async def test_open_notepad(self, result: VirtualNodeTestResult):
        # Close any existing windows first
        await self.engine.execute("hotkey", node_id=NODE_ID, keys=["alt", "f4"])
        await asyncio.sleep(0.5)

        await self.engine.execute("hotkey", node_id=NODE_ID, keys=["win", "r"])
        await asyncio.sleep(1.5)
        await self.engine.execute("type", node_id=NODE_ID, text="notepad")
        await asyncio.sleep(0.3)
        await self.engine.execute("key", node_id=NODE_ID, key="enter")
        await asyncio.sleep(2)

        if not await self.assert_text("Notepad"):
            result.error = "Notepad didn't open"

    async def test_type_notepad(self, result: VirtualNodeTestResult):
        await self.engine.execute("type", node_id=NODE_ID, text="Ozma RPA test OK")
        await asyncio.sleep(1)
        text = await self.read_text()
        if "RPA" not in text and "test" not in text.lower():
            result.error = "Typed text not found in Notepad"
        await self.screenshot("notepad_typed")

    async def test_close_notepad(self, result: VirtualNodeTestResult):
        await self.engine.execute("hotkey", node_id=NODE_ID, keys=["alt", "f4"])
        await asyncio.sleep(1)
        # Don't save dialog — press N or Tab+Enter
        await self.engine.execute("key", node_id=NODE_ID, key="n")
        await asyncio.sleep(1)

    # ── Phase 5: Agent Runtime ────────────────────────────────────────

    async def test_start_agent(self, result: VirtualNodeTestResult):
        await self.engine.execute("hotkey", node_id=NODE_ID, keys=["win", "r"])
        await asyncio.sleep(1)
        await self.engine.execute("type", node_id=NODE_ID, text="cmd")
        await asyncio.sleep(0.3)
        await self.engine.execute("key", node_id=NODE_ID, key="enter")
        await asyncio.sleep(2)
        await self.type_cmd("start C:\\ozma-agent\\dist\\ozma-agent.exe run --no-tray --no-capture --debug")
        await asyncio.sleep(5)

    async def test_agent_running(self, result: VirtualNodeTestResult):
        await self.type_cmd('tasklist /fi "imagename eq ozma-agent.exe"')
        await asyncio.sleep(2)
        if not await self.assert_text("ozma-agent"):
            result.error = "Agent process not found in tasklist"

    async def test_stop_agent(self, result: VirtualNodeTestResult):
        await self.type_cmd("taskkill /im ozma-agent.exe /f")
        await asyncio.sleep(2)


# ── Main ──────────────────────────────────────────────────────────────────

async def main():
    quick = "--quick" in sys.argv
    verbose = "--verbose" in sys.argv

    test = IntegrationTest(quick=quick, verbose=verbose)
    success = await test.run_all()
    sys.exit(0 if success else 1)


if __name__ == "__main__":
    asyncio.run(main())
