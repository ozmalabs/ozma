#!/usr/bin/env python3
"""
Unit tests for AudioOutputManager — multi-output enable/disable,
delay timing, avahi discovery parsing, sink name generation.

Does NOT require running PipeWire or network.  Tests the logic layer;
subprocess calls are mocked.
"""

import asyncio
import json
import sys
import unittest
from unittest.mock import AsyncMock, patch, MagicMock

sys.path.insert(0, "controller")

from audio_outputs import (
    AudioOutput, AudioOutputManager, _sanitize, _avahi_unescape,
)


class TestAudioOutput(unittest.TestCase):

    def test_to_dict(self):
        o = AudioOutput(
            id="test", name="Test", protocol="raop",
            host="192.168.1.10", port=7000,
            enabled=True, delay_ms=150.0,
        )
        d = o.to_dict()
        self.assertEqual(d["id"], "test")
        self.assertEqual(d["protocol"], "raop")
        self.assertTrue(d["enabled"])
        self.assertAlmostEqual(d["delay_ms"], 150.0)

    def test_default_values(self):
        o = AudioOutput(id="x", name="X", protocol="local")
        self.assertFalse(o.enabled)
        self.assertAlmostEqual(o.delay_ms, 0.0)
        self.assertEqual(o.pw_sink_name, "")


class TestSanitize(unittest.TestCase):

    def test_basic(self):
        self.assertEqual(_sanitize("Hello World"), "hello-world")

    def test_special_chars(self):
        self.assertEqual(_sanitize("Kimberley's MacBook Air"), "kimberley-s-macbook-air")

    def test_already_clean(self):
        self.assertEqual(_sanitize("ozma-vm1"), "ozma-vm1")


class TestAvahiUnescape(unittest.TestCase):

    def test_decimal_escape(self):
        # avahi uses decimal \NNN for non-ASCII bytes
        # \032 = space (ASCII 32)
        result = _avahi_unescape("Hello\\032World")
        self.assertEqual(result, "Hello World")

    def test_apostrophe_escape(self):
        # \039 = apostrophe (ASCII 39)
        result = _avahi_unescape("Kimberley\\039s MacBook")
        self.assertEqual(result, "Kimberley's MacBook")

    def test_no_escapes(self):
        self.assertEqual(_avahi_unescape("plain text"), "plain text")

    def test_at_sign_in_raop(self):
        result = _avahi_unescape("AABBCCDD@Speaker Name")
        self.assertEqual(result, "AABBCCDD@Speaker Name")


class TestAudioOutputManager(unittest.TestCase):

    def setUp(self):
        self.mgr = AudioOutputManager()
        self.events: list[tuple[str, dict]] = []

        async def capture(t, d):
            self.events.append((t, d))

        self.mgr.on_event = capture

    def _run(self, coro):
        return asyncio.get_event_loop().run_until_complete(coro)

    # ── Defaults ─────────────────────────────────────────────────────────────

    def test_local_output_exists(self):
        outputs = self.mgr.list_outputs()
        self.assertEqual(len(outputs), 1)
        self.assertEqual(outputs[0]["id"], "local")
        self.assertTrue(outputs[0]["enabled"])

    def test_get_selected_sink_default(self):
        # Local output has empty pw_sink_name = PW default
        result = self.mgr.get_selected_sink()
        self.assertIsNone(result)  # empty string → None

    # ── Enable / disable ─────────────────────────────────────────────────────

    def test_enable_local(self):
        self.mgr._outputs["local"].enabled = False
        ok = self._run(self.mgr.enable_output("local"))
        self.assertTrue(ok)
        self.assertTrue(self.mgr._outputs["local"].enabled)

    def test_disable_local(self):
        ok = self._run(self.mgr.disable_output("local"))
        self.assertTrue(ok)
        self.assertFalse(self.mgr._outputs["local"].enabled)

    def test_enable_nonexistent(self):
        ok = self._run(self.mgr.enable_output("nonexistent"))
        self.assertFalse(ok)

    def test_multi_enable(self):
        self.mgr._outputs["raop"] = AudioOutput(
            id="raop", name="AirPlay", protocol="raop",
            host="10.0.0.1", port=7000, pw_sink_name="raop-sink",
        )
        self._run(self.mgr.enable_output("raop"))
        # Both local and raop should be enabled
        enabled = [o for o in self.mgr._outputs.values() if o.enabled]
        self.assertEqual(len(enabled), 2)

    # ── Delay ────────────────────────────────────────────────────────────────

    def test_set_delay(self):
        ok = self._run(self.mgr.set_delay("local", 200.0))
        self.assertTrue(ok)
        self.assertAlmostEqual(self.mgr._outputs["local"].delay_ms, 200.0)

    def test_set_delay_negative_clamped(self):
        self._run(self.mgr.set_delay("local", -50))
        self.assertAlmostEqual(self.mgr._outputs["local"].delay_ms, 0.0)

    def test_set_delay_nonexistent(self):
        ok = self._run(self.mgr.set_delay("nope", 100))
        self.assertFalse(ok)

    def test_delay_emits_event(self):
        self._run(self.mgr.set_delay("local", 150))
        changed = [e for e in self.events if e[0] == "audio.output_changed"]
        self.assertTrue(len(changed) >= 1)
        self.assertAlmostEqual(changed[-1][1]["delay_ms"], 150.0)

    # ── get_enabled_sinks ────────────────────────────────────────────────────

    def test_get_enabled_sinks(self):
        self.mgr._outputs["rtp"] = AudioOutput(
            id="rtp", name="RTP", protocol="rtp",
            pw_sink_name="ozma-rtp-test", enabled=True,
        )
        sinks = self.mgr.get_enabled_sinks()
        # local (empty string) + rtp
        self.assertIn("", sinks)
        self.assertIn("ozma-rtp-test", sinks)

    def test_get_enabled_sinks_none_enabled(self):
        self.mgr._outputs["local"].enabled = False
        sinks = self.mgr.get_enabled_sinks()
        self.assertEqual(len(sinks), 0)

    # ── connect_source / disconnect_all ──────────────────────────────────────

    @patch("audio_outputs._run_cmd", new_callable=AsyncMock, return_value=(0, ""))
    @patch("audio_outputs._pactl", new_callable=AsyncMock, return_value="default-sink")
    async def _test_connect_source_no_delay(self, mock_pactl, mock_run):
        """Direct link (no delay) when delay_ms == 0."""
        await self.mgr.connect_source("ozma-vm1")
        # Should call pw-link for the local output
        mock_run.assert_called()
        args = mock_run.call_args[0][0]
        self.assertEqual(args[0], "pw-link")

    def test_connect_source_no_delay(self):
        self._run(self._test_connect_source_no_delay())

    @patch("audio_outputs.asyncio.create_subprocess_exec", new_callable=AsyncMock)
    @patch("audio_outputs._pactl", new_callable=AsyncMock, return_value="default-sink")
    async def _test_connect_source_with_delay(self, mock_pactl, mock_subproc):
        """pw-loopback launched when delay_ms > 0."""
        mock_proc = MagicMock()
        mock_proc.pid = 12345
        mock_proc.returncode = None
        mock_subproc.return_value = mock_proc

        self.mgr._outputs["local"].delay_ms = 200.0
        await self.mgr.connect_source("ozma-vm1")

        mock_subproc.assert_called_once()
        call_args = mock_subproc.call_args[0]
        self.assertEqual(call_args[0], "pw-loopback")
        self.assertIn("--delay", call_args)
        self.assertIn("0.2", call_args)

    def test_connect_source_with_delay(self):
        self._run(self._test_connect_source_with_delay())

    # ── add_output ───────────────────────────────────────────────────────────

    def test_add_output(self):
        o = AudioOutput(id="roc1", name="ROC Receiver", protocol="roc",
                        host="10.0.0.5", port=10001)
        self._run(self.mgr.add_output(o))
        self.assertIn("roc1", self.mgr._outputs)
        discovered = [e for e in self.events if e[0] == "audio.output_discovered"]
        self.assertEqual(len(discovered), 1)

    # ── RAOP discovery parsing ───────────────────────────────────────────────

    @patch("audio_outputs.asyncio.create_subprocess_exec")
    def test_discover_raop(self, mock_exec):
        """Parse avahi-browse output for AirPlay receivers."""
        avahi_output = (
            "=;eth0;IPv4;AABB@Living Room;_raop._tcp;local;host.local;192.168.1.50;7000;\n"
        )
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(avahi_output.encode(), b""))
        mock_exec.return_value = mock_proc

        self._run(self.mgr._discover_raop())

        self.assertIn("raop-living-room", self.mgr._outputs)
        o = self.mgr._outputs["raop-living-room"]
        self.assertEqual(o.name, "Living Room")
        self.assertEqual(o.host, "192.168.1.50")
        self.assertEqual(o.port, 7000)


class TestAudioOutputManagerLifecycle(unittest.TestCase):

    def _run(self, coro):
        return asyncio.get_event_loop().run_until_complete(coro)

    def test_stop_kills_delay_procs(self):
        mgr = AudioOutputManager()
        mock_proc = MagicMock()
        mock_proc.returncode = None
        mock_proc.terminate = MagicMock()
        mock_proc.wait = AsyncMock()
        mgr._outputs["local"]._delay_proc = mock_proc

        self._run(mgr.stop())
        mock_proc.terminate.assert_called_once()


if __name__ == "__main__":
    unittest.main()
