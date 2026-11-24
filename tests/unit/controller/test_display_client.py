"""Tests for the reusable Python protobuf Display client."""

from __future__ import annotations

import asyncio
import json

import pytest

import display_client
import display_protocol as dp
from display_client import DisplayClient
from ozma.v1 import action_pb2, data_pb2, scene_pb2

pytestmark = pytest.mark.unit

websockets = pytest.importorskip("websockets")

TEST_LAYOUT = {
    "fields": {
        "node.cpu_temp": {"type": "real", "unit": "celsius"},
        "node.online": {"type": "bool"},
        "scenario_name": {"type": "string"},
    },
    "widgets": [
        {"type": "gauge", "metric": "node.cpu_temp"},
        {"type": "icon", "metric": "node.online"},
        {"type": "label", "text": "{scenario_name}"},
    ],
}


def _definition() -> data_pb2.DisplayDefinition:
    return dp.build_definition(TEST_LAYOUT, "unit-screen", 3)


class FakeController:
    """Minimal in-process controller screen server for one client."""

    def __init__(self, definition: data_pb2.DisplayDefinition) -> None:
        self.definition = definition
        self.url = ""
        self.registrations: list[dict] = []
        self.inbound: asyncio.Queue[bytes] = asyncio.Queue()
        self.registration = asyncio.Event()
        self.ws = None

    async def handler(self, ws, *_path) -> None:
        self.ws = ws
        try:
            async for message in ws:
                if isinstance(message, (bytes, bytearray, memoryview)):
                    await self.inbound.put(bytes(message))
                    continue
                try:
                    payload = json.loads(message)
                except (TypeError, ValueError):
                    continue
                if not isinstance(payload, dict):
                    continue
                self.registrations.append(payload)
                if payload.get("type") == "register":
                    await ws.send(
                        json.dumps(
                            {
                                "type": "registered",
                                "device_id": payload.get("device_id"),
                                "protocol": "protobuf",
                                "wire_format": "protobuf",
                            }
                        )
                    )
                    if self.definition is not None:
                        await ws.send(
                            dp.encode_frame(dp.TAG_DEFINITION, self.definition)
                        )
                    self.registration.set()
        finally:
            self.ws = None

    async def send(self, message) -> None:
        assert self.ws is not None, "client is not connected"
        await self.ws.send(message)


@pytest.fixture
async def controller():
    fake = FakeController(_definition())
    server = await websockets.serve(fake.handler, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    fake.url = f"ws://127.0.0.1:{port}"
    try:
        yield fake
    finally:
        server.close()
        await server.wait_closed()


async def _start(client: DisplayClient, controller: FakeController):
    task = asyncio.create_task(client.run())
    await asyncio.wait_for(controller.registration.wait(), 2)
    await asyncio.wait_for(client.wait_for_definition(), 2)
    return task


async def _stop(client: DisplayClient, task: asyncio.Task) -> None:
    await client.close()
    try:
        await asyncio.wait_for(task, 2)
    except asyncio.CancelledError:  # pragma: no cover - defensive
        pass


def test_module_supports_package_and_top_level_imports():
    import controller.display_client as packaged

    assert hasattr(packaged, "DisplayClient")
    assert hasattr(display_client, "DisplayClient")
    assert packaged.DisplayClient.__name__ == display_client.DisplayClient.__name__


async def test_registration_definition_and_data_updates(controller):
    definitions: list[data_pb2.DisplayDefinition] = []
    data_updates: list[tuple[dict, data_pb2.DataUpdate]] = []
    data_event = asyncio.Event()

    async def on_data(values, update):
        data_updates.append((values, update))
        data_event.set()

    client = DisplayClient(
        controller.url,
        device_id="unit-display",
        name="Unit Display",
        width=320,
        height=240,
        capabilities=["gauge", "protobuf"],
        on_definition=definitions.append,
        on_data=on_data,
    )
    task = await _start(client, controller)
    try:
        registration = controller.registrations[0]
        assert registration["type"] == "register"
        assert registration["protocol"] == "protobuf"
        assert registration["wire_format"] == "protobuf"
        assert registration["device_id"] == "unit-display"
        assert registration["name"] == "Unit Display"
        assert registration["width"] == 320
        assert registration["height"] == 240
        assert registration["capabilities"] == ["gauge", "protobuf"]

        assert [item.id for item in definitions] == ["unit-screen"]
        assert client.definition is not None
        assert client.definition_version == 3
        assert client.definition_document is not None
        assert client.definition_document["id"] == "unit-screen"
        assert client.status == "connected"

        update = dp.build_data_update(
            controller.definition,
            {
                "node.cpu_temp": 61.5,
                "node.online": True,
                "scenario_name": "Gaming",
            },
        )
        await controller.send(dp.encode_frame(dp.TAG_DATA_UPDATE, update))
        await asyncio.wait_for(data_event.wait(), 2)

        values, message = data_updates[-1]
        assert message.definition_version == 3
        assert values["node.cpu_temp"] == pytest.approx(61.5)
        assert values["node.online"] is True
        assert values["scenario_name"] == "Gaming"
        assert client.values_by_key == values
        cpu_id = client.field_by_key["node.cpu_temp"].id
        assert client.values[cpu_id] == pytest.approx(61.5)
        assert client.latest_update is message
    finally:
        await _stop(client, task)


async def test_send_action_is_decoded_by_the_server(controller):
    client = DisplayClient(controller.url, device_id="action-display")
    task = await _start(client, controller)
    try:
        key = await client.send_action(1, {"scenario": "gaming"})
        assert key >= 1

        frame = await asyncio.wait_for(controller.inbound.get(), 2)
        tag, invoke = dp.decode_frame(frame)
        assert tag == dp.TAG_ACTION_INVOKE
        assert isinstance(invoke, action_pb2.ActionInvoke)
        assert invoke.action_id == 1
        assert invoke.idempotency_key == key
        assert dp.action_invoke_params(invoke) == {"scenario": "gaming"}

        assert await client.send_action(2, idempotency_key=1234) == 1234
        frame = await asyncio.wait_for(controller.inbound.get(), 2)
        _, invoke = dp.decode_frame(frame)
        assert invoke.action_id == 2
        assert invoke.idempotency_key == 1234
        assert dp.action_invoke_params(invoke) == {}
    finally:
        await _stop(client, task)


async def test_malformed_and_unknown_frames_do_not_kill_the_client(controller):
    statuses: list[dict] = []
    data_event = asyncio.Event()
    client = DisplayClient(
        controller.url,
        device_id="robust-display",
        on_data=lambda values, update: data_event.set(),
        on_status=statuses.append,
    )
    task = await _start(client, controller)
    try:
        for frame in (b"", b"\x09\x00", b"\x05\x08", b"\xff\xff"):
            await controller.send(frame)
        await controller.send("not json")

        update = dp.build_data_update(
            controller.definition, {"node.online": False}
        )
        await controller.send(dp.encode_frame(dp.TAG_DATA_UPDATE, update))
        await asyncio.wait_for(data_event.wait(), 2)

        assert client.connected is True
        assert client.status == "connected"
        assert client.values_by_key["node.online"] is False
        assert any(item.get("state") == "error" for item in statuses)
    finally:
        await _stop(client, task)


async def test_refs_patches_scenes_and_actions_update_state(controller):
    scenes: list[scene_pb2.SceneSnapshot] = []
    actions: list[action_pb2.ActionInvoke] = []
    scene_event = asyncio.Event()
    action_event = asyncio.Event()

    def on_scene(snapshot):
        scenes.append(snapshot)
        scene_event.set()

    def on_action(invoke):
        actions.append(invoke)
        action_event.set()

    client = DisplayClient(
        controller.url,
        device_id="state-display",
        on_scene=on_scene,
        on_action=on_action,
    )
    task = await _start(client, controller)
    try:
        await controller.send(
            dp.encode_frame(
                dp.TAG_DEFINITION_REF,
                data_pb2.DefinitionRef(id="unit-screen", version=4, hash=b"r" * 32),
            )
        )
        await controller.send(
            dp.encode_frame(
                dp.TAG_DEFINITION_PATCH,
                data_pb2.DefinitionPatch(
                    definition_id="unit-screen",
                    base_version=4,
                    version=5,
                    patch_json=b"[]",
                ),
            )
        )

        snapshot = scene_pb2.SceneSnapshot(revision=7, roots=[1])
        snapshot.nodes.add(id=1)
        await controller.send(
            dp.encode_frame(dp.TAG_SCENE_SNAPSHOT, snapshot)
        )
        await asyncio.wait_for(scene_event.wait(), 2)

        assert client.last_definition_ref.version == 4
        assert client.last_patch.version == 5
        assert client.definition_id == "unit-screen"
        assert client.definition_version == 5
        assert client.scene.revision == 7
        assert [item.revision for item in scenes] == [7]

        invoke = action_pb2.ActionInvoke(
            action_id=9, params_json=b'{"on": true}'
        )
        await controller.send(dp.encode_frame(dp.TAG_ACTION_INVOKE, invoke))
        await asyncio.wait_for(action_event.wait(), 2)
        assert actions[-1].action_id == 9
        assert dp.action_invoke_params(actions[-1]) == {"on": True}
    finally:
        await _stop(client, task)
