"""Registration and driver-selection tests for the screen WebSocket server."""

from __future__ import annotations

import json

import pytest

import display_protocol as dp
from ozma.v1 import action_pb2, data_pb2
from screen_drivers import NativeRenderProtobufDriver
from screen_manager import NativeRenderDriver
from screen_server import ScreenWebSocketServer

pytestmark = pytest.mark.unit


class FakeWebSocket:
    """Minimal async-iterator/send WebSocket double."""

    def __init__(self, messages=()):
        self._messages = list(messages)
        self.sent: list[object] = []

    def __aiter__(self):
        self._iterator = iter(self._messages)
        return self

    async def __anext__(self):
        try:
            return next(self._iterator)
        except StopIteration:
            raise StopAsyncIteration

    async def send(self, message):
        self.sent.append(message)


class FakeScreenManager:
    """Captures screens/drivers across register and disconnect."""

    def __init__(self):
        from screen_widgets import BUILTIN_LAYOUTS

        self._layouts = BUILTIN_LAYOUTS
        self.screens = {}
        self.registered = []
        self.unregistered = []
        self.drivers_unregistered = []
        self.actions = []
        self.interactions = []
        self.last_screen = None

    def register_screen(self, screen):
        screen.layout = self._layouts.get(screen.layout_id)
        self.screens[screen.id] = screen
        self.registered.append(screen)
        self.last_screen = screen

    def unregister_screen(self, screen_id):
        self.screens.pop(screen_id, None)
        self.unregistered.append(screen_id)

    def unregister_native_driver(self, device_id):
        self.drivers_unregistered.append(device_id)

    async def handle_action_invoke(self, device_id, invoke):
        self.actions.append((device_id, invoke))

    async def handle_interaction_event(self, device_id, event):
        self.interactions.append((device_id, event))


async def _run_register(manager, registration):
    ws = FakeWebSocket([json.dumps(registration)])
    server = ScreenWebSocketServer(manager)
    await server._handle_client(ws)
    return ws, manager.last_screen


async def test_registration_defaults_to_json_driver():
    manager = FakeScreenManager()
    ws, screen = await _run_register(manager, {
        "type": "register",
        "device_id": "json-device",
        "layout": "panel-status",
    })

    assert isinstance(screen.driver, NativeRenderDriver)
    assert not isinstance(screen.driver, NativeRenderProtobufDriver)
    assert isinstance(ws.sent[0], str)
    assert json.loads(ws.sent[0])["type"] == "layout"

    ack = json.loads(ws.sent[1])
    assert ack["type"] == "registered"
    assert ack["protocol"] == "json"
    assert ack["wire_format"] == "json"
    assert manager.unregistered == ["json-device"]
    assert manager.drivers_unregistered == ["json-device"]


async def test_registration_selects_protobuf_driver_only_when_requested():
    manager = FakeScreenManager()
    ws, screen = await _run_register(manager, {
        "type": "register",
        "device_id": "proto-device",
        "protocol": "protobuf",
        "layout": "panel-status",
    })

    assert isinstance(screen.driver, NativeRenderProtobufDriver)
    assert screen.driver.wire_format == "protobuf"

    tag, definition = dp.decode_frame(ws.sent[0])
    assert tag == dp.TAG_DEFINITION
    assert isinstance(definition, data_pb2.DisplayDefinition)
    assert definition.id == "proto-device"
    assert definition.fields

    ack = json.loads(ws.sent[1])
    assert ack["protocol"] == "protobuf"
    assert ack["wire_format"] == "protobuf"
    assert manager.unregistered == ["proto-device"]
    assert manager.drivers_unregistered == ["proto-device"]


async def test_json_registration_sent_as_binary_still_uses_json_driver():
    manager = FakeScreenManager()
    ws = FakeWebSocket([
        json.dumps({
            "type": "register",
            "device_id": "binary-json",
            "layout": "panel-status",
        }).encode(),
    ])
    server = ScreenWebSocketServer(manager)
    await server._handle_client(ws)

    assert isinstance(manager.last_screen.driver, NativeRenderDriver)
    assert not isinstance(manager.last_screen.driver, NativeRenderProtobufDriver)
    assert json.loads(ws.sent[0])["type"] == "layout"


async def test_wire_format_alias_selects_protobuf():
    manager = FakeScreenManager()
    ws, screen = await _run_register(manager, {
        "type": "register",
        "device_id": "proto-alias",
        "wire_format": "protobuf",
        "layout": "panel-status",
    })

    assert isinstance(screen.driver, NativeRenderProtobufDriver)
    assert dp.decode_frame(ws.sent[0])[0] == dp.TAG_DEFINITION


async def test_inbound_action_frame_reaches_controller_callback():
    manager = FakeScreenManager()
    invoke = action_pb2.ActionInvoke(action_id=11, params_json=b'{"view": "cpu"}')
    action_frame = dp.encode_frame(dp.TAG_ACTION_INVOKE, invoke)

    ws = FakeWebSocket([
        json.dumps({
            "type": "register",
            "device_id": "proto-action",
            "protocol": "protobuf",
            "layout": "panel-status",
        }),
        action_frame,
    ])
    server = ScreenWebSocketServer(manager)
    await server._handle_client(ws)

    assert len(manager.actions) == 1
    device_id, received = manager.actions[0]
    assert device_id == "proto-action"
    assert received.action_id == 11
    assert dp.action_invoke_params(received) == {"view": "cpu"}


async def test_inbound_interaction_frame_reaches_controller_callback():
    manager = FakeScreenManager()
    event = action_pb2.InteractionEvent(node_id=21)
    event.semantic.name = "media.play_pause"
    event_frame = dp.encode_frame(dp.TAG_INTERACTION_EVENT, event)

    ws = FakeWebSocket([
        json.dumps({
            "type": "register",
            "device_id": "proto-interaction",
            "protocol": "protobuf",
            "layout": "panel-status",
        }),
        event_frame,
    ])
    server = ScreenWebSocketServer(manager)
    await server._handle_client(ws)

    assert len(manager.interactions) == 1
    device_id, received = manager.interactions[0]
    assert device_id == "proto-interaction"
    assert received.node_id == 21
    assert received.semantic.name == "media.play_pause"


async def test_malformed_binary_frame_is_ignored_and_client_unregistered():
    manager = FakeScreenManager()
    ws = FakeWebSocket([
        json.dumps({
            "type": "register",
            "device_id": "proto-bad",
            "protocol": "protobuf",
            "layout": "panel-status",
        }),
        b"\x09not-a-known-tag",
        b"\x05\x08",  # truncated varint
    ])
    server = ScreenWebSocketServer(manager)
    await server._handle_client(ws)

    assert manager.actions == []
    assert manager.unregistered == ["proto-bad"]
    assert manager.drivers_unregistered == ["proto-bad"]


async def test_protobuf_driver_sends_definition_once_and_deltas_only():
    driver = NativeRenderProtobufDriver("delta-device")
    ws = FakeWebSocket()
    driver.set_websocket(ws)
    layout = {
        "id": "panel",
        "widgets": [
            {"type": "gauge", "metric": "node.cpu_temp"},
            {"type": "label", "text": "{scenario_name}"},
        ],
    }

    await driver.push_layout(layout)
    await driver.push_layout(layout)  # unchanged layout must not resend
    assert len(ws.sent) == 1
    tag, definition = dp.decode_frame(ws.sent[0])
    assert tag == dp.TAG_DEFINITION
    assert definition.version == 1

    await driver.push_data({"node.cpu_temp": 50.0, "scenario_name": "Gaming"})
    await driver.push_data({"node.cpu_temp": 50.0, "scenario_name": "Gaming"})
    assert len(ws.sent) == 2  # no update when nothing changed

    await driver.push_data({"node.cpu_temp": 51.5, "scenario_name": "Gaming"})
    assert len(ws.sent) == 3
    tag, update = dp.decode_frame(ws.sent[-1])
    assert tag == dp.TAG_DATA_UPDATE
    by_key = {field.key: field.id for field in definition.fields}
    assert [value.field_id for value in update.values] == [by_key["node.cpu_temp"]]
    assert update.values[0].WhichOneof("value") == "real"

    # A changed layout produces a new definition version.
    changed_layout = dict(layout)
    changed_layout["widgets"] = [
        {"type": "gauge", "metric": "node.cpu_temp"},
        {"type": "gauge", "metric": "node.gpu_temp"},
    ]
    await driver.push_layout(changed_layout)
    assert len(ws.sent) == 4
    tag, definition2 = dp.decode_frame(ws.sent[-1])
    assert tag == dp.TAG_DEFINITION
    assert definition2.version == 2
