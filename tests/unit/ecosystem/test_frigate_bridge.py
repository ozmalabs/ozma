# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for ecosystem/frigate-tools ozma_bridge.py — _on_message event classification."""
import asyncio
import json
import sys
from pathlib import Path
from types import ModuleType
from unittest.mock import MagicMock

import pytest

pytestmark = pytest.mark.unit

# ── Stub paho.mqtt (not installed in all dev envs) ────────────────────────────

def _stub_mqtt():
    mqtt_mod = ModuleType("paho.mqtt.client")
    mqtt_mod.Client = MagicMock
    mqtt_mod.MQTTMessage = MagicMock
    mqtt_mod.CallbackAPIVersion = MagicMock()
    mqtt_mod.CallbackAPIVersion.VERSION2 = "V2"
    paho_mod = ModuleType("paho")
    paho_mqtt_mod = ModuleType("paho.mqtt")
    sys.modules.setdefault("paho", paho_mod)
    sys.modules.setdefault("paho.mqtt", paho_mqtt_mod)
    sys.modules["paho.mqtt.client"] = mqtt_mod

_stub_mqtt()

_BRIDGE_DIR = Path(__file__).parent.parent.parent.parent / "ecosystem" / "frigate-tools"
sys.path.insert(0, str(_BRIDGE_DIR))

from frigate_tools.ozma_bridge import OzmaBridge, BridgeConfig  # noqa: E402


# ── Helpers ───────────────────────────────────────────────────────────────────

def _make_bridge(prefix="frigate") -> OzmaBridge:
    cfg = BridgeConfig(mqtt_topic_prefix=prefix)
    bridge = OzmaBridge(cfg)
    # Provide a mock loop that reports as running and calls put_nowait synchronously
    mock_loop = MagicMock()
    mock_loop.is_running.return_value = True
    mock_loop.call_soon_threadsafe = lambda fn, *args: fn(*args)
    bridge._loop = mock_loop
    return bridge


def _make_msg(topic: str, payload) -> MagicMock:
    """Build a mock paho MQTTMessage."""
    msg = MagicMock()
    msg.topic = topic
    if isinstance(payload, (dict, list)):
        msg.payload = json.dumps(payload).encode()
    elif isinstance(payload, bool):
        msg.payload = json.dumps(payload).encode()
    elif isinstance(payload, bytes):
        msg.payload = payload
    else:
        msg.payload = str(payload).encode()
    return msg


def _capture_events(bridge: OzmaBridge, msg) -> list[dict]:
    """Call _on_message and drain the queue synchronously."""
    bridge._on_message(MagicMock(), None, msg)
    events = []
    while True:
        try:
            events.append(bridge._ws_queue.get_nowait())
        except asyncio.QueueEmpty:
            break
    return events


# ── Doorbell button events ────────────────────────────────────────────────────

class TestDoorbellEvents:
    def test_doorbell_press_classified_as_doorbell(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/front_door/doorbell", True)
        events = _capture_events(bridge, msg)
        assert len(events) == 1
        assert events[0]["kind"] == "doorbell"

    def test_doorbell_camera_extracted_correctly(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/back_garden/doorbell", True)
        events = _capture_events(bridge, msg)
        assert events[0]["camera"] == "back_garden"

    def test_doorbell_release_not_forwarded(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/front_door/doorbell", False)
        events = _capture_events(bridge, msg)
        assert len(events) == 0

    def test_doorbell_payload_one_forwarded(self):
        """Some brokers publish 1 instead of true."""
        bridge = _make_bridge()
        msg = _make_msg("frigate/front_door/doorbell", 1)
        events = _capture_events(bridge, msg)
        assert len(events) == 1

    def test_doorbell_payload_zero_not_forwarded(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/front_door/doorbell", 0)
        events = _capture_events(bridge, msg)
        assert len(events) == 0

    def test_doorbell_custom_prefix(self):
        bridge = _make_bridge(prefix="nvr")
        msg = _make_msg("nvr/front_door/doorbell", True)
        events = _capture_events(bridge, msg)
        assert events[0]["kind"] == "doorbell"


# ── Person recognized events ──────────────────────────────────────────────────

class TestPersonRecognizedEvents:
    def _person_event(self, camera="front_door", sub_label="Matt", event_type="new"):
        return {
            "type": event_type,
            "after": {
                "label": "person",
                "sub_label": sub_label,
                "camera": camera,
                "score": 0.92,
                "top_score": 0.95,
            },
        }

    def test_person_recognized_on_new_event(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/events", self._person_event())
        events = _capture_events(bridge, msg)
        assert events[0]["kind"] == "person_recognized"

    def test_person_recognized_on_update_event(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/events", self._person_event(event_type="update"))
        events = _capture_events(bridge, msg)
        assert events[0]["kind"] == "person_recognized"

    def test_person_name_extracted(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/events", self._person_event(sub_label="Alice"))
        events = _capture_events(bridge, msg)
        assert events[0]["person"] == "Alice"

    def test_camera_extracted(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/events", self._person_event(camera="side_gate"))
        events = _capture_events(bridge, msg)
        assert events[0]["camera"] == "side_gate"

    def test_person_not_recognized_no_sub_label(self):
        """Person detected but no sub_label → plain event, not person_recognized."""
        bridge = _make_bridge()
        payload = {
            "type": "new",
            "after": {
                "label": "person",
                "sub_label": None,
                "camera": "front_door",
            },
        }
        msg = _make_msg("frigate/events", payload)
        events = _capture_events(bridge, msg)
        assert events[0]["kind"] == "event"
        assert "person" not in events[0]

    def test_non_person_label_not_recognized(self):
        """Car detection should not produce person_recognized."""
        bridge = _make_bridge()
        payload = {
            "type": "new",
            "after": {
                "label": "car",
                "sub_label": "Toyota",
                "camera": "driveway",
            },
        }
        msg = _make_msg("frigate/events", payload)
        events = _capture_events(bridge, msg)
        assert events[0]["kind"] == "event"

    def test_end_event_type_not_recognized(self):
        """'end' events should not trigger person_recognized even with sub_label."""
        bridge = _make_bridge()
        msg = _make_msg("frigate/events", self._person_event(event_type="end"))
        events = _capture_events(bridge, msg)
        assert events[0]["kind"] == "event"

    def test_sub_label_coerced_to_string(self):
        """sub_label could theoretically be a number — must come out as string."""
        bridge = _make_bridge()
        payload = {
            "type": "new",
            "after": {
                "label": "person",
                "sub_label": 42,   # unusual but guard against it
                "camera": "front_door",
            },
        }
        msg = _make_msg("frigate/events", payload)
        events = _capture_events(bridge, msg)
        if events[0]["kind"] == "person_recognized":
            assert isinstance(events[0]["person"], str)


# ── Generic events (reviews, etc.) ────────────────────────────────────────────

class TestGenericEvents:
    def test_reviews_topic_classified_as_event(self):
        bridge = _make_bridge()
        payload = {"camera": "back_garden", "score": 0.8}
        msg = _make_msg("frigate/reviews", payload)
        events = _capture_events(bridge, msg)
        assert events[0]["kind"] == "event"

    def test_event_includes_topic(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/reviews", {"camera": "x"})
        events = _capture_events(bridge, msg)
        assert events[0]["topic"] == "frigate/reviews"

    def test_event_includes_payload(self):
        bridge = _make_bridge()
        payload = {"camera": "front_door", "score": 0.9}
        msg = _make_msg("frigate/events", payload)
        events = _capture_events(bridge, msg)
        assert events[0]["payload"] == payload

    def test_non_json_payload_forwarded_as_string(self):
        """Malformed JSON should still forward (as a string, not crash)."""
        bridge = _make_bridge()
        msg = _make_msg("frigate/reviews", b"not-json!")
        events = _capture_events(bridge, msg)
        assert len(events) == 1
        assert isinstance(events[0]["payload"], str)


# ── Event queue integration ───────────────────────────────────────────────────

class TestEventQueue:
    def test_event_enqueued_in_ws_queue(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/front_door/doorbell", True)
        bridge._on_message(MagicMock(), None, msg)
        assert bridge._ws_queue.qsize() == 1

    def test_doorbell_release_not_enqueued(self):
        bridge = _make_bridge()
        msg = _make_msg("frigate/front_door/doorbell", False)
        bridge._on_message(MagicMock(), None, msg)
        assert bridge._ws_queue.qsize() == 0

    def test_multiple_events_all_enqueued(self):
        bridge = _make_bridge()
        for _ in range(3):
            msg = _make_msg("frigate/front_door/doorbell", True)
            bridge._on_message(MagicMock(), None, msg)
        assert bridge._ws_queue.qsize() == 3

    def test_no_loop_does_not_enqueue(self):
        """If the event loop isn't set, message is silently dropped (not a crash)."""
        bridge = _make_bridge()
        bridge._loop = None
        msg = _make_msg("frigate/front_door/doorbell", True)
        bridge._on_message(MagicMock(), None, msg)
        assert bridge._ws_queue.qsize() == 0
