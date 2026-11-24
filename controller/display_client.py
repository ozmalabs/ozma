# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Reusable Python client for the Ozma protobuf Display channel.

The controller's native screen WebSocket server (default port ``7391``) accepts
a JSON registration frame and then speaks the binary ``[tag][protobuf]``
Display protocol implemented by :mod:`controller.display_protocol`:

    Client -> Controller   JSON registration with ``"protocol": "protobuf"``
    Controller -> Client   DisplayDefinition, DataUpdate, ActionInvoke, ...
    Client -> Controller   ActionInvoke, InteractionEvent

This module is intentionally small and dependency-light.  It is suitable for
Linux SBCs, e-ink panels, Stream Deck hosts, and other devices that can run
Python and the ``websockets`` package.  Devices that cannot run Python can use
the same wire protocol from any language; see
``docs/display-device-adapters.md``.

Example::

    import asyncio
    from display_client import DisplayClient

    async def main():
        client = DisplayClient(
            "ws://controller.local:7391",
            device_id="kitchen-panel",
            on_definition=lambda definition: print(definition.id),
            on_data=lambda values, update: print(values),
        )
        await client.run()

    asyncio.run(main())

Callback signatures (all optional, sync or async):

``on_definition(definition)``
    A complete :class:`ozma.v1.data_pb2.DisplayDefinition` arrived.
``on_data(values, update)``
    A compact ``DataUpdate`` arrived.  ``values`` is a dict keyed by field key
    (or by stringified field id for fields absent from the definition) and
    ``update`` is the raw ``DataUpdate`` message.
``on_action(invoke)``
    The controller invoked an action on this device.  ``invoke`` is an
    ``ActionInvoke`` message; use ``display_protocol.action_invoke_to_dict``
    for a plain dict.
``on_interaction(event)``
    An ``InteractionEvent`` arrived (for adapters that receive input from the
    controller side).
``on_scene(snapshot)``
    A complete ``SceneSnapshot`` arrived.
``on_status(status)``
    Connection and diagnostic status.  ``status`` is a dict with at least
    ``state`` and ``url`` keys.

Callbacks may raise; the client logs the exception and keeps the connection
alive so a broken renderer cannot take down the display channel.
"""

from __future__ import annotations

import argparse
import asyncio
import inspect
import json
import logging
import sys
import uuid
from pathlib import Path
from typing import Any, Callable

# ``display_protocol`` is normally imported as a top-level module when the
# controller directory is on sys.path (the controller does this), but the
# package-relative form must work as well for ``controller.display_client``.
# Running this file directly (``python controller/display_client.py``) also
# needs the controller directory on sys.path.
if __package__:
    from . import display_protocol
else:  # pragma: no cover - exercised by the CLI and top-level imports
    try:
        import display_protocol
    except ModuleNotFoundError:
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        import display_protocol

try:
    import websockets
except ImportError:  # pragma: no cover - the tests skip when this is missing
    websockets = None  # type: ignore[assignment]

# ``display_protocol`` puts ``controller/proto_gen`` on sys.path, so these
# imports resolve to the checked-in generated bindings.
from ozma.v1 import action_pb2, data_pb2, scene_pb2

log = logging.getLogger("ozma.display_client")

DEFAULT_URL = "ws://localhost:7391"
DEFAULT_CAPABILITIES = ("protobuf",)
_DEFAULT_WIDTH = 320
_DEFAULT_HEIGHT = 240


class DisplayClient:
    """Async client for the controller's protobuf Display WebSocket channel."""

    def __init__(
        self,
        url: str = DEFAULT_URL,
        *,
        device_id: str | None = None,
        name: str | None = None,
        width: int = _DEFAULT_WIDTH,
        height: int = _DEFAULT_HEIGHT,
        capabilities: list[str] | tuple[str, ...] | None = None,
        refresh_hz: float | None = 10,
        layout: str | None = None,
        on_definition: Callable[..., Any] | None = None,
        on_data: Callable[..., Any] | None = None,
        on_action: Callable[..., Any] | None = None,
        on_interaction: Callable[..., Any] | None = None,
        on_scene: Callable[..., Any] | None = None,
        on_status: Callable[..., Any] | None = None,
        reconnect: bool = True,
        min_backoff: float = 1.0,
        max_backoff: float = 30.0,
        backoff_factor: float = 2.0,
        connect_timeout: float = 10.0,
    ) -> None:
        self.url = url
        self.device_id = device_id or f"python-{uuid.uuid4().hex[:8]}"
        self.name = name or self.device_id
        self.width = int(width)
        self.height = int(height)
        self.capabilities = tuple(capabilities or DEFAULT_CAPABILITIES)
        self.refresh_hz = refresh_hz
        self.layout = layout

        self.on_definition = on_definition
        self.on_data = on_data
        self.on_action = on_action
        self.on_interaction = on_interaction
        self.on_scene = on_scene
        self.on_status = on_status

        self.reconnect = reconnect
        self.min_backoff = max(0.0, float(min_backoff))
        self.max_backoff = max(self.min_backoff, float(max_backoff))
        self.backoff_factor = max(1.0, float(backoff_factor))
        self.connect_timeout = float(connect_timeout)

        # Protocol state.
        self.status = "idle"
        self.definition: data_pb2.DisplayDefinition | None = None
        self.definition_document: dict[str, Any] | None = None
        self.definition_id = ""
        self.definition_version = 0
        self.definition_hash = b""
        self.fields: list[data_pb2.FieldDef] = []
        self.field_by_id: dict[int, data_pb2.FieldDef] = {}
        self.field_by_key: dict[str, data_pb2.FieldDef] = {}
        self.values: dict[int, Any] = {}
        self.values_by_key: dict[str, Any] = {}
        self.latest_update: data_pb2.DataUpdate | None = None
        self.scene: scene_pb2.SceneSnapshot | None = None
        self.last_scene_commit: scene_pb2.SceneCommit | None = None
        self.last_definition_ref: data_pb2.DefinitionRef | None = None
        self.last_patch: data_pb2.DefinitionPatch | None = None
        self.last_action: action_pb2.ActionInvoke | None = None
        self.last_interaction: action_pb2.InteractionEvent | None = None

        self._ws: Any = None
        self._closed = False
        self._definition_event = asyncio.Event()
        self._idempotency = 0

    # ------------------------------------------------------------------
    # Connection lifecycle
    # ------------------------------------------------------------------

    @property
    def connected(self) -> bool:
        """Whether a WebSocket connection is currently open."""
        return self._ws is not None

    def registration_payload(self) -> dict[str, Any]:
        """Return the JSON registration frame sent on every (re)connect."""
        payload: dict[str, Any] = {
            "type": "register",
            "device_id": self.device_id,
            "name": self.name,
            "width": self.width,
            "height": self.height,
            "protocol": "protobuf",
            "wire_format": "protobuf",
            "capabilities": list(self.capabilities),
        }
        if self.refresh_hz is not None:
            payload["refresh_hz"] = self.refresh_hz
        if self.layout:
            payload["layout"] = self.layout
        return payload

    async def connect(self) -> Any:
        """Open one connection and send the registration frame.

        Reconnection is handled by :meth:`run`; this method raises if the
        connection cannot be established.
        """
        if self._ws is not None:
            return self._ws
        if self._closed:
            raise RuntimeError("DisplayClient is closed")
        if websockets is None:
            raise RuntimeError(
                "the 'websockets' package is required for DisplayClient"
            )

        await self._set_status("connecting")
        try:
            ws = await websockets.connect(
                self.url,
                max_size=display_protocol.MAX_FRAME_LEN + 4096,
                open_timeout=self.connect_timeout,
            )
        except Exception as exc:
            await self._set_status("error", error=str(exc))
            raise

        self._ws = ws
        try:
            registration = json.dumps(
                self.registration_payload(), separators=(",", ":")
            )
            await ws.send(registration)
        except Exception:
            self._ws = None
            try:
                await ws.close()
            except Exception:  # pragma: no cover - defensive
                pass
            raise

        await self._set_status("connected")
        return ws

    async def run(self) -> None:
        """Receive frames until closed, reconnecting with bounded backoff."""
        if self._closed:
            return

        backoff = self.min_backoff
        while not self._closed:
            ws = None
            try:
                if self._ws is None:
                    await self.connect()
                ws = self._ws
                backoff = self.min_backoff
                async for message in ws:
                    await self._handle_message(message)
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                if not self._closed:
                    log.warning("Display connection error: %s", exc)
                    await self._set_status("error", error=str(exc))
            finally:
                ws, self._ws = self._ws, None
                if ws is not None:
                    try:
                        await ws.close()
                    except Exception:  # pragma: no cover - defensive
                        pass

            if self._closed or not self.reconnect:
                break

            await self._set_status("reconnecting", delay=backoff)
            try:
                await asyncio.sleep(backoff)
            except asyncio.CancelledError:
                raise
            backoff = min(backoff * self.backoff_factor, self.max_backoff)

        await self._set_status("closed")

    # ``run_forever`` is the familiar websockets-style spelling.
    run_forever = run

    async def close(self) -> None:
        """Close the connection and stop any reconnect loop."""
        self._closed = True
        ws, self._ws = self._ws, None
        if ws is not None:
            try:
                await ws.close()
            except Exception:  # pragma: no cover - defensive
                pass
        await self._set_status("closed")

    # ------------------------------------------------------------------
    # Outbound messages
    # ------------------------------------------------------------------

    async def send_action(
        self,
        action_id: int,
        params: Any = None,
        idempotency_key: int | None = None,
    ) -> int:
        """Send an ``ActionInvoke`` frame and return its idempotency key.

        ``params`` may be a mapping, a JSON string, or raw bytes.  When
        ``idempotency_key`` is omitted a monotonically increasing key is
        generated so the controller can de-duplicate retries.
        """
        ws = self._require_connection()
        key = (
            int(idempotency_key)
            if idempotency_key is not None
            else self._next_idempotency_key()
        )
        invoke = action_pb2.ActionInvoke(
            action_id=int(action_id),
            idempotency_key=key,
        )
        if params is not None:
            invoke.params = self._params_bytes(params)
        frame = display_protocol.encode_frame(
            display_protocol.TAG_ACTION_INVOKE, invoke
        )
        await ws.send(frame)
        return key

    async def send_interaction(
        self, event: action_pb2.InteractionEvent
    ) -> None:
        """Send an ``InteractionEvent`` frame (pointer, key, or semantic)."""
        if not isinstance(event, action_pb2.InteractionEvent):
            raise TypeError("event must be an ozma.v1 InteractionEvent")
        ws = self._require_connection()
        frame = display_protocol.encode_frame(
            display_protocol.TAG_INTERACTION_EVENT, event
        )
        await ws.send(frame)

    # ------------------------------------------------------------------
    # Inbound messages
    # ------------------------------------------------------------------

    async def _handle_message(self, message: Any) -> None:
        if isinstance(message, (bytes, bytearray, memoryview)):
            await self._handle_frame(bytes(message))
            return
        if not isinstance(message, str):
            await self._notify(
                "on_status",
                {"state": "error", "url": self.url, "error": "ignored non-text message"},
            )
            return

        try:
            payload = json.loads(message)
        except (TypeError, ValueError):
            await self._notify(
                "on_status",
                {"state": "error", "url": self.url, "error": "ignored non-JSON text frame"},
            )
            return

        if isinstance(payload, dict) and payload.get("type") == "registered":
            if payload.get("device_id"):
                self.device_id = str(payload["device_id"])
            await self._notify(
                "on_status",
                {"state": "registered", "url": self.url, "message": payload},
            )
        elif isinstance(payload, dict):
            await self._notify(
                "on_status",
                {"state": "message", "url": self.url, "message": payload},
            )

    async def _handle_frame(self, raw: bytes) -> None:
        try:
            tag, message = display_protocol.decode_frame(raw)
        except Exception as exc:
            # Unknown tags, truncated varints, and empty frames are expected
            # from misbehaving peers; keep the connection alive.
            log.debug("Ignoring malformed display frame (%d bytes): %s", len(raw), exc)
            await self._notify(
                "on_status",
                {"state": "error", "url": self.url, "error": f"ignored display frame: {exc}"},
            )
            return

        if tag == display_protocol.TAG_DEFINITION:
            await self._handle_definition(message)
        elif tag == display_protocol.TAG_DEFINITION_REF:
            await self._handle_definition_ref(message)
        elif tag == display_protocol.TAG_DEFINITION_PATCH:
            await self._handle_definition_patch(message)
        elif tag == display_protocol.TAG_SCENE_COMMIT:
            self.last_scene_commit = message
        elif tag == display_protocol.TAG_DATA_UPDATE:
            await self._handle_data_update(message)
        elif tag == display_protocol.TAG_ACTION_INVOKE:
            self.last_action = message
            await self._notify("on_action", message)
        elif tag == display_protocol.TAG_INTERACTION_EVENT:
            self.last_interaction = message
            await self._notify("on_interaction", message)
            if message.WhichOneof("event") == "action":
                self.last_action = message.action
                await self._notify("on_action", message.action)
        elif tag == display_protocol.TAG_SCENE_SNAPSHOT:
            self.scene = message
            await self._notify("on_scene", message)
        else:  # pragma: no cover - decode_frame rejects unknown tags
            await self._notify(
                "on_status",
                {"state": "error", "url": self.url, "error": f"unhandled display tag {tag}"},
            )

    async def _handle_definition(self, definition: data_pb2.DisplayDefinition) -> None:
        self.definition = definition
        self.definition_id = definition.id
        self.definition_version = definition.version
        self.definition_hash = bytes(definition.hash)
        self.fields = list(definition.fields)
        self.field_by_id = {field.id: field for field in self.fields}
        self.field_by_key = {field.key: field for field in self.fields}
        self.definition_document = self._parse_definition_json(definition)
        self._definition_event.set()
        await self._notify("on_definition", definition)

    async def _handle_definition_ref(self, ref: data_pb2.DefinitionRef) -> None:
        self.last_definition_ref = ref
        if ref.id:
            self.definition_id = ref.id
        if ref.version:
            self.definition_version = ref.version
        if ref.hash:
            self.definition_hash = bytes(ref.hash)

    async def _handle_definition_patch(
        self, patch: data_pb2.DefinitionPatch
    ) -> None:
        # A patch is recorded but not applied to the compiled field table.
        # Devices that need the new table should wait for the controller to
        # send a full DisplayDefinition, as the screen server does on layout
        # changes.  The version/hash still advance so adapters can tell that
        # their cached definition is stale.
        self.last_patch = patch
        if patch.definition_id:
            self.definition_id = patch.definition_id
        if patch.version:
            self.definition_version = patch.version
        if patch.hash:
            self.definition_hash = bytes(patch.hash)

    async def _handle_data_update(self, update: data_pb2.DataUpdate) -> None:
        self.latest_update = update
        if update.definition_version:
            self.definition_version = update.definition_version

        values: dict[str, Any] = {}
        for field_value in update.values:
            field = self.field_by_id.get(field_value.field_id)
            key = field.key if field is not None else str(field_value.field_id)
            value = self._field_value(field_value, field)
            self.values[field_value.field_id] = value
            self.values_by_key[key] = value
            values[key] = value

        await self._notify("on_data", values, update)

    @staticmethod
    def _field_value(
        field_value: data_pb2.FieldValue, field: data_pb2.FieldDef | None
    ) -> Any:
        kind = field_value.WhichOneof("value")
        if kind is None:
            return None
        raw = getattr(field_value, kind)
        if (
            kind in {"integer", "unsigned"}
            and field is not None
            and field.HasField("scale")
            and field.scale
        ):
            return raw / field.scale
        return raw

    @staticmethod
    def _parse_definition_json(
        definition: data_pb2.DisplayDefinition,
    ) -> dict[str, Any] | None:
        if not definition.definition_json:
            return None
        try:
            document = json.loads(bytes(definition.definition_json).decode("utf-8"))
        except (UnicodeDecodeError, ValueError):
            return None
        return document if isinstance(document, dict) else None

    # ------------------------------------------------------------------
    # Convenience and internals
    # ------------------------------------------------------------------

    async def wait_for_definition(
        self, timeout: float | None = None
    ) -> data_pb2.DisplayDefinition | None:
        """Wait until a full definition is cached (or ``timeout`` expires)."""
        if self.definition is None:
            await asyncio.wait_for(self._definition_event.wait(), timeout)
        return self.definition

    def _require_connection(self) -> Any:
        if self._ws is None:
            raise ConnectionError("DisplayClient is not connected")
        return self._ws

    def _next_idempotency_key(self) -> int:
        self._idempotency += 1
        if self._idempotency > 0xFFFFFFFFFFFFFFFF:
            self._idempotency = 1
        return self._idempotency

    @staticmethod
    def _params_bytes(params: Any) -> bytes:
        if isinstance(params, str):
            return params.encode("utf-8")
        if isinstance(params, (bytes, bytearray, memoryview)):
            return bytes(params)
        return display_protocol.canonical_json_bytes(params)

    async def _set_status(self, state: str, **info: Any) -> None:
        self.status = state
        payload = {"state": state, "url": self.url, "device_id": self.device_id}
        payload.update(info)
        await self._notify("on_status", payload)

    async def _notify(self, callback_name: str, *args: Any) -> None:
        callback = getattr(self, callback_name, None)
        if callback is None:
            return
        try:
            result = callback(*args)
            if inspect.isawaitable(result):
                await result
        except asyncio.CancelledError:
            raise
        except Exception:
            log.exception("DisplayClient %s callback failed", callback_name)


# ----------------------------------------------------------------------
# Small command-line display client
# ----------------------------------------------------------------------


def _print_definition(definition: data_pb2.DisplayDefinition) -> None:
    print(
        f"definition {definition.id} v{definition.version} "
        f"({len(definition.fields)} fields, {len(definition.actions)} actions)"
    )
    for field in definition.fields:
        type_name = display_protocol.TYPE_NAMES.get(field.type, str(field.type))
        unit = f" [{field.unit}]" if field.HasField("unit") else ""
        print(f"  {field.id:>4}  {field.key}  {type_name}{unit}")


def _print_data(values: dict[str, Any], update: data_pb2.DataUpdate) -> None:
    sequence = (
        f" seq={update.sequence}" if update.HasField("sequence") else ""
    )
    rendered = ", ".join(f"{key}={value!r}" for key, value in values.items())
    print(f"data v{update.definition_version}{sequence}: {rendered}")


def _print_status(status: dict[str, Any]) -> None:
    state = status.get("state", "?")
    detail = status.get("error") or status.get("message") or ""
    if isinstance(detail, dict):
        detail = detail.get("type", "")
    print(f"[{state}] {status.get('url', '')} {detail}".rstrip())


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Connect to the Ozma controller protobuf Display channel."
    )
    parser.add_argument(
        "--url",
        default=DEFAULT_URL,
        help=f"controller screen WebSocket URL (default: {DEFAULT_URL})",
    )
    parser.add_argument("--device-id", default=None, help="stable device identifier")
    parser.add_argument("--name", default=None, help="human-readable device name")
    parser.add_argument("--width", type=int, default=_DEFAULT_WIDTH)
    parser.add_argument("--height", type=int, default=_DEFAULT_HEIGHT)
    parser.add_argument(
        "--layout",
        default="panel-status",
        help="controller layout id to request (default: panel-status)",
    )
    parser.add_argument(
        "--capabilities",
        nargs="*",
        default=None,
        help="renderer capabilities to advertise",
    )
    parser.add_argument(
        "--no-reconnect",
        action="store_true",
        help="exit instead of retrying after a disconnect",
    )
    parser.add_argument("--min-backoff", type=float, default=1.0)
    parser.add_argument("--max-backoff", type=float, default=30.0)
    return parser


async def _run_cli(args: argparse.Namespace) -> None:
    client = DisplayClient(
        args.url,
        device_id=args.device_id,
        name=args.name,
        width=args.width,
        height=args.height,
        layout=args.layout,
        capabilities=args.capabilities,
        reconnect=not args.no_reconnect,
        min_backoff=args.min_backoff,
        max_backoff=args.max_backoff,
        on_definition=_print_definition,
        on_data=_print_data,
        on_status=_print_status,
    )
    try:
        await client.run()
    finally:
        await client.close()


def main(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    try:
        asyncio.run(_run_cli(args))
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
