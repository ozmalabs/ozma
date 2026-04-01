#!/usr/bin/env python3
"""
Unit tests for PipeWireWatcher — JSON parsing, node/link tracking,
name lookup, volume/mute state, snapshot.

Does NOT require a running PipeWire instance.  Tests feed mock JSON
directly into the watcher's _process_batch method.
"""

import asyncio
import json
import sys
import unittest

sys.path.insert(0, "controller")

from pipewire_watcher import PipeWireWatcher, PWNode, PWLink, _get


# ── Mock pw-dump data ────────────────────────────────────────────────────────

MOCK_NODE_SINK = {
    "id": 42,
    "type": "PipeWire:Interface:Node",
    "version": 3,
    "permissions": ["r", "w", "x", "m"],
    "info": {
        "max-input-ports": 65,
        "max-output-ports": 0,
        "n-input-ports": 2,
        "n-output-ports": 0,
        "state": "idle",
        "error": None,
        "props": {
            "node.name": "alsa_output.usb-test",
            "node.nick": "Test Headphones",
            "media.class": "Audio/Sink",
            "device.id": 10,
        },
        "params": {
            "Props": [
                {
                    "channelVolumes": [0.75, 0.75],
                    "softVolumes": [0.75, 0.75],
                    "mute": False,
                    "softMute": False,
                }
            ]
        },
    },
}

MOCK_NODE_SOURCE = {
    "id": 43,
    "type": "PipeWire:Interface:Node",
    "version": 3,
    "permissions": ["r", "w", "x", "m"],
    "info": {
        "max-input-ports": 0,
        "max-output-ports": 65,
        "n-input-ports": 0,
        "n-output-ports": 2,
        "state": "idle",
        "error": None,
        "props": {
            "node.name": "ozma-vm1",
            "media.class": "Audio/Source",
        },
        "params": {
            "Props": [
                {
                    "channelVolumes": [1.0, 1.0],
                    "mute": False,
                }
            ]
        },
    },
}

MOCK_LINK = {
    "id": 100,
    "type": "PipeWire:Interface:Link",
    "version": 3,
    "permissions": ["r", "x"],
    "info": {
        "output-node-id": 43,
        "input-node-id": 42,
        "state": "active",
        "error": None,
        "props": {
            "link.output.node": 43,
            "link.output.port": 80,
            "link.input.node": 42,
            "link.input.port": 90,
        },
    },
}

MOCK_NODE_IRRELEVANT = {
    "id": 99,
    "type": "PipeWire:Interface:Node",
    "version": 3,
    "info": {
        "state": "idle",
        "props": {
            "node.name": "video-source",
            "media.class": "Video/Source",
        },
        "params": {},
    },
}

MOCK_DELETE = {
    "id": 42,
    "type": "PipeWire:Interface:Node",
}


def _batch(*items) -> str:
    return json.dumps(list(items))


class TestPipeWireWatcher(unittest.TestCase):

    def setUp(self):
        self.watcher = PipeWireWatcher()
        self.events: list[tuple[str, dict]] = []

        async def capture(t, d):
            self.events.append((t, d))

        self.watcher.on_event = capture

    def _run(self, coro):
        return asyncio.get_event_loop().run_until_complete(coro)

    # ── Node parsing ─────────────────────────────────────────────────────────

    def test_parse_audio_sink(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SINK)))
        self.assertIn(42, self.watcher.nodes)
        node = self.watcher.nodes[42]
        self.assertEqual(node.name, "alsa_output.usb-test")
        self.assertEqual(node.media_class, "Audio/Sink")
        self.assertAlmostEqual(node.volume, 0.75)
        self.assertFalse(node.mute)
        self.assertEqual(node.channels, 2)
        self.assertTrue(node.is_sink)
        self.assertFalse(node.is_source)

    def test_parse_audio_source(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SOURCE)))
        self.assertIn(43, self.watcher.nodes)
        node = self.watcher.nodes[43]
        self.assertEqual(node.name, "ozma-vm1")
        self.assertTrue(node.is_source)

    def test_ignores_non_audio_nodes(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_IRRELEVANT)))
        self.assertNotIn(99, self.watcher.nodes)

    def test_name_index(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SINK)))
        self.assertIs(self.watcher.find_node("alsa_output.usb-test"),
                      self.watcher.nodes[42])
        self.assertIs(self.watcher.find_node("Test Headphones"),
                      self.watcher.nodes[42])

    def test_alias_map(self):
        self.watcher.alias_map = {"alsa_output.usb-test": "Headphones"}
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SINK)))
        node = self.watcher.find_node("Headphones")
        self.assertIsNotNone(node)
        self.assertEqual(node.id, 42)

    # ── Link parsing ─────────────────────────────────────────────────────────

    def test_parse_link(self):
        self._run(self.watcher._process_batch(
            _batch(MOCK_NODE_SINK, MOCK_NODE_SOURCE, MOCK_LINK)
        ))
        self.assertIn(100, self.watcher.links)
        link = self.watcher.links[100]
        self.assertEqual(link.output_node_id, 43)
        self.assertEqual(link.input_node_id, 42)
        self.assertEqual(link.state, "active")

        # Node link tracking
        self.assertIn(42, self.watcher.nodes[43].outlinks)
        self.assertIn(43, self.watcher.nodes[42].inlinks)

    def test_is_linked(self):
        self._run(self.watcher._process_batch(
            _batch(MOCK_NODE_SINK, MOCK_NODE_SOURCE, MOCK_LINK)
        ))
        self.assertTrue(self.watcher.is_linked("ozma-vm1", "alsa_output.usb-test"))
        self.assertFalse(self.watcher.is_linked("alsa_output.usb-test", "ozma-vm1"))

    # ── Deletion ─────────────────────────────────────────────────────────────

    def test_delete_node(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SINK)))
        self.assertIn(42, self.watcher.nodes)

        self._run(self.watcher._process_batch(_batch(MOCK_DELETE)))
        self.assertNotIn(42, self.watcher.nodes)
        self.assertIsNone(self.watcher.find_node("alsa_output.usb-test"))

    def test_delete_link(self):
        self._run(self.watcher._process_batch(
            _batch(MOCK_NODE_SINK, MOCK_NODE_SOURCE, MOCK_LINK)
        ))
        delete_link = {"id": 100, "type": "PipeWire:Interface:Link"}
        self._run(self.watcher._process_batch(_batch(delete_link)))
        self.assertNotIn(100, self.watcher.links)
        self.assertNotIn(42, self.watcher.nodes[43].outlinks)

    # ── Volume change detection ──────────────────────────────────────────────

    def test_volume_change_event(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SINK)))
        self.events.clear()

        # Update volume
        updated = json.loads(json.dumps(MOCK_NODE_SINK))
        updated["info"]["params"]["Props"][0]["channelVolumes"] = [0.5, 0.5]
        updated["info"]["params"]["Props"][0]["softVolumes"] = [0.5, 0.5]
        self._run(self.watcher._process_batch(_batch(updated)))

        vol_events = [e for e in self.events if e[0] == "audio.volume_changed"]
        self.assertEqual(len(vol_events), 1)
        self.assertAlmostEqual(vol_events[0][1]["volume"], 0.5)

    def test_mute_change_event(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SINK)))
        self.events.clear()

        updated = json.loads(json.dumps(MOCK_NODE_SINK))
        updated["info"]["params"]["Props"][0]["mute"] = True
        self._run(self.watcher._process_batch(_batch(updated)))

        vol_events = [e for e in self.events if e[0] == "audio.volume_changed"]
        self.assertEqual(len(vol_events), 1)
        self.assertTrue(vol_events[0][1]["mute"])

    # ── Snapshot ─────────────────────────────────────────────────────────────

    def test_snapshot(self):
        self._run(self.watcher._process_batch(
            _batch(MOCK_NODE_SINK, MOCK_NODE_SOURCE, MOCK_LINK)
        ))
        snap = self.watcher.snapshot()
        self.assertIn("nodes", snap)
        self.assertIn("links", snap)
        self.assertEqual(len(snap["nodes"]), 2)
        self.assertEqual(len(snap["links"]), 1)
        self.assertIn("alsa_output.usb-test", snap["nodes"])
        self.assertIn("ozma-vm1", snap["nodes"])

    # ── Sinks / sources properties ───────────────────────────────────────────

    def test_sinks_and_sources(self):
        self._run(self.watcher._process_batch(
            _batch(MOCK_NODE_SINK, MOCK_NODE_SOURCE)
        ))
        self.assertIn(42, self.watcher.sinks)
        self.assertNotIn(43, self.watcher.sinks)
        self.assertIn(43, self.watcher.sources)
        self.assertNotIn(42, self.watcher.sources)

    # ── Events fired on new node ─────────────────────────────────────────────

    def test_node_online_event(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SINK)))
        online_events = [e for e in self.events if e[0] == "audio.node_online"]
        self.assertEqual(len(online_events), 1)
        self.assertEqual(online_events[0][1]["name"], "alsa_output.usb-test")

    def test_node_offline_event(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SINK)))
        self.events.clear()
        self._run(self.watcher._process_batch(_batch(MOCK_DELETE)))
        offline_events = [e for e in self.events if e[0] == "audio.node_offline"]
        self.assertEqual(len(offline_events), 1)

    # ── _get helper ──────────────────────────────────────────────────────────

    def test_get_helper(self):
        d = {"a": {"b": {"c": 42}}}
        self.assertEqual(_get(d, "a", "b", "c"), 42)
        self.assertIsNone(_get(d, "a", "x", "c"))
        self.assertEqual(_get(d, "a", "x", default="nope"), "nope")

    # ── JSON parse error resilience ──────────────────────────────────────────

    def test_bad_json_skipped(self):
        self._run(self.watcher._process_batch("not json at all"))
        self.assertEqual(len(self.watcher.nodes), 0)

    # ── to_dict ──────────────────────────────────────────────────────────────

    def test_node_to_dict(self):
        self._run(self.watcher._process_batch(_batch(MOCK_NODE_SINK)))
        d = self.watcher.nodes[42].to_dict()
        self.assertEqual(d["id"], 42)
        self.assertEqual(d["name"], "alsa_output.usb-test")
        self.assertAlmostEqual(d["volume"], 0.75)
        self.assertFalse(d["mute"])


if __name__ == "__main__":
    unittest.main()
