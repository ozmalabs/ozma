# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Real-time PipeWire graph watcher using ``pw-dump -m -N``.

Streams JSON updates from PipeWire, maintains a live model of audio nodes,
links, and their state (volume, mute).  Provides reactive callbacks instead
of polling, and volume/mute control via ``pw-cli set-param``.

Inspired by surfacepresser-run's PipewireService, rewritten for ozma's
async architecture with proper dataclasses and clean event handling.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shutil
from dataclasses import dataclass, field
from typing import Any, Callable, Coroutine

log = logging.getLogger("ozma.pw_watcher")

# Types we track from PipeWire
_TRACKED_TYPES = frozenset({
    "PipeWire:Interface:Node",
    "PipeWire:Interface:Link",
    "PipeWire:Interface:Device",
})

# Media classes we consider audio-relevant
_AUDIO_CLASSES = frozenset({
    "Audio/Sink",
    "Audio/Source",
    "Audio/Source/Virtual",
    "Audio/Duplex",
    "Stream/Input/Audio",
    "Stream/Output/Audio",
})


def _get(d: dict, *keys: str, default: Any = None) -> Any:
    """Safely traverse nested dicts."""
    for k in keys:
        if not isinstance(d, dict):
            return default
        d = d.get(k, default)
    return d


@dataclass
class PWNode:
    """A tracked PipeWire audio node."""

    id: int
    name: str  # Primary name (node.name)
    names: list[str] = field(default_factory=list)  # All name variants
    media_class: str = ""
    volume: float = 1.0
    mute: bool = False
    channels: int = 2
    inlinks: set[int] = field(default_factory=set)   # Node IDs feeding in
    outlinks: set[int] = field(default_factory=set)   # Node IDs fed to
    device_id: int | None = None
    props: dict = field(default_factory=dict, repr=False)

    @property
    def is_sink(self) -> bool:
        return self.media_class in ("Audio/Sink", "Stream/Input/Audio")

    @property
    def is_source(self) -> bool:
        return self.media_class in ("Audio/Source", "Audio/Source/Virtual",
                                    "Stream/Output/Audio")

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "names": self.names,
            "media_class": self.media_class,
            "volume": round(self.volume, 4),
            "mute": self.mute,
            "channels": self.channels,
            "inlinks": sorted(self.inlinks),
            "outlinks": sorted(self.outlinks),
        }


@dataclass
class PWLink:
    """A tracked PipeWire link between two nodes."""

    id: int
    output_node_id: int
    input_node_id: int
    state: str = "unknown"


# Callback type: async def callback(event_type: str, data: dict)
Callback = Callable[[str, dict], Coroutine[Any, Any, None]]


class PipeWireWatcher:
    """
    Streams ``pw-dump -m -N``, maintains live PipeWire graph state,
    and fires async callbacks on changes.

    Usage::

        watcher = PipeWireWatcher(alias_map={"alsa_output.usb-...": "Headphones"})
        watcher.on_event = my_callback  # async def (event_type, data)
        await watcher.start()
        ...
        node = watcher.find_node("ozma-vm1")
        await watcher.set_volume("ozma-vm1", 0.5)
        ...
        await watcher.stop()
    """

    def __init__(self, alias_map: dict[str, str] | None = None) -> None:
        self.nodes: dict[int, PWNode] = {}      # id → PWNode
        self.links: dict[int, PWLink] = {}       # id → PWLink
        self.name_index: dict[str, PWNode] = {}  # any name → PWNode
        self.alias_map: dict[str, str] = alias_map or {}

        # Single callback for all events; set by consumer
        self.on_event: Callback | None = None

        self._task: asyncio.Task | None = None
        self._ready = asyncio.Event()

        # Optional PipeWire runtime dir override (for root processes that need
        # to reach a user-owned PipeWire socket at /run/user/<uid>/pipewire-0).
        # Reads OZMA_PIPEWIRE_RUNTIME_DIR env var; set XDG_RUNTIME_DIR when
        # spawning pw-dump/pw-cli subprocesses.
        _rt = os.environ.get("OZMA_PIPEWIRE_RUNTIME_DIR", "")
        self._pw_env: dict[str, str] | None = None
        if _rt:
            self._pw_env = {**os.environ, "XDG_RUNTIME_DIR": _rt, "PIPEWIRE_RUNTIME_DIR": _rt}

    @property
    def available(self) -> bool:
        return shutil.which("pw-dump") is not None

    @property
    def sinks(self) -> dict[int, PWNode]:
        return {k: v for k, v in self.nodes.items() if v.is_sink}

    @property
    def sources(self) -> dict[int, PWNode]:
        return {k: v for k, v in self.nodes.items() if v.is_source}

    async def start(self) -> None:
        """Launch pw-dump subprocess and begin streaming."""
        if not self.available:
            log.warning("pw-dump not found — PipeWire watcher disabled")
            return
        self._task = asyncio.create_task(self._run(), name="pw-watcher")
        # Wait for initial dump to be processed
        try:
            await asyncio.wait_for(self._ready.wait(), timeout=5.0)
        except asyncio.TimeoutError:
            log.warning("PipeWire watcher: initial dump timed out")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    def find_node(self, name: str) -> PWNode | None:
        """Look up a node by any of its names or aliases."""
        # Check alias first
        canonical = self.alias_map.get(name, name)
        return self.name_index.get(canonical) or self.name_index.get(name)

    def is_linked(self, source_name: str, sink_name: str) -> bool:
        """Check if source is currently linked to sink."""
        src = self.find_node(source_name)
        snk = self.find_node(sink_name)
        if not src or not snk:
            return False
        return snk.id in src.outlinks

    def get_links_for_node(self, node_name: str) -> list[dict]:
        """Return all links involving a node (by name)."""
        node = self.find_node(node_name)
        if not node:
            return []
        result = []
        for link in self.links.values():
            if link.output_node_id == node.id or link.input_node_id == node.id:
                out_node = self.nodes.get(link.output_node_id)
                in_node = self.nodes.get(link.input_node_id)
                result.append({
                    "link_id": link.id,
                    "source": out_node.name if out_node else str(link.output_node_id),
                    "sink": in_node.name if in_node else str(link.input_node_id),
                    "state": link.state,
                })
        return result

    # ── Volume / mute control ────────────────────────────────────────────────

    async def set_volume(self, node_name: str, volume: float) -> bool:
        """Set volume (linear 0.0-1.0+) on a node. Returns True on success."""
        node = self.find_node(node_name)
        if not node:
            log.warning("set_volume: node %r not found", node_name)
            return False
        props = {
            "channelVolumes": [volume] * node.channels,
            "softVolumes": [volume] * node.channels,
        }
        rc, err = await _run_pw_cmd(
            ["pw-cli", "set-param", str(node.id), "Props", json.dumps(props)],
            env=self._pw_env,
        )
        if rc != 0:
            log.warning("set_volume %s=%.3f failed: %s", node_name, volume, err)
            return False
        # Also set on the device if this node has one
        if node.device_id and node.device_id in self.nodes:
            await _run_pw_cmd(
                ["pw-cli", "set-param", str(node.device_id), "Props", json.dumps(props)],
                env=self._pw_env,
            )
        return True

    async def set_mute(self, node_name: str, mute: bool) -> bool:
        """Set mute state on a node. Returns True on success."""
        node = self.find_node(node_name)
        if not node:
            log.warning("set_mute: node %r not found", node_name)
            return False
        props = {"mute": mute, "softMute": mute}
        rc, err = await _run_pw_cmd(
            ["pw-cli", "set-param", str(node.id), "Props", json.dumps(props)],
            env=self._pw_env,
        )
        if rc != 0:
            log.warning("set_mute %s=%s failed: %s", node_name, mute, err)
            return False
        return True

    # ── Snapshot for API ─────────────────────────────────────────────────────

    def snapshot(self) -> dict[str, Any]:
        """Return full audio state for API/WebSocket."""
        return {
            "nodes": {n.name: n.to_dict() for n in self.nodes.values()
                      if n.media_class in _AUDIO_CLASSES},
            "links": [
                {
                    "id": l.id,
                    "source": self.nodes[l.output_node_id].name
                        if l.output_node_id in self.nodes else str(l.output_node_id),
                    "sink": self.nodes[l.input_node_id].name
                        if l.input_node_id in self.nodes else str(l.input_node_id),
                    "state": l.state,
                }
                for l in self.links.values()
            ],
        }

    # ── Internal: poll-based pw-dump ────────────────────────────────────────

    async def _run(self) -> None:
        """Poll pw-dump -N periodically to track PipeWire state changes."""
        first = True
        while True:
            try:
                proc = await asyncio.create_subprocess_exec(
                    "pw-dump", "-N",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.DEVNULL,
                    env=self._pw_env,
                )
                stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10.0)
                raw = stdout.decode("utf-8", errors="replace")
                await self._process_batch(raw)

                if first:
                    first = False
                    self._ready.set()
                    log.info("PipeWire watcher: initial scan found %d audio nodes, %d links",
                             len(self.nodes), len(self.links))

                await asyncio.sleep(1.0)

            except asyncio.CancelledError:
                return
            except asyncio.TimeoutError:
                log.debug("pw-dump timed out, retrying")
                await asyncio.sleep(2.0)
            except Exception:
                log.exception("PipeWire watcher error, retrying in 5s")
                await asyncio.sleep(5.0)

    async def _process_batch(self, raw: str) -> None:
        """Parse a JSON array from pw-dump and update state."""
        try:
            items = json.loads(raw)
        except json.JSONDecodeError:
            log.debug("pw-dump: JSON parse error, skipping batch")
            return

        for item in items:
            obj_type = item.get("type", "")
            obj_id = item.get("id")
            if obj_id is None or obj_type not in _TRACKED_TYPES:
                continue

            # Deletion: no "info" key present
            if "info" not in item:
                await self._handle_delete(obj_id, obj_type)
                continue

            if obj_type == "PipeWire:Interface:Node":
                await self._handle_node(item)
            elif obj_type == "PipeWire:Interface:Link":
                await self._handle_link(item)
            # Devices tracked minimally — only for volume cascading

    async def _handle_node(self, item: dict) -> None:
        """Add or update an audio node."""
        obj_id = item["id"]
        props = _get(item, "info", "props", default={})
        media_class = props.get("media.class", "")

        if media_class not in _AUDIO_CLASSES:
            return

        # Extract all possible names
        names = []
        for key in ("node.name", "node.nick", "application.name",
                     "device.product.name"):
            val = props.get(key)
            if val and val not in names:
                names.append(val)
        if not names:
            return

        primary_name = names[0]

        # Apply alias mapping
        for i, n in enumerate(names):
            if n in self.alias_map:
                names.append(self.alias_map[n])
                if i == 0:
                    primary_name = self.alias_map[n]

        # Volume/mute from params
        params_props = _get(item, "info", "params", "Props", default=[])
        volume = 1.0
        mute = False
        channels = 2
        if params_props and isinstance(params_props, list) and params_props:
            p = params_props[0] if isinstance(params_props[0], dict) else {}
            ch_vols = p.get("channelVolumes") or p.get("softVolumes")
            if ch_vols and isinstance(ch_vols, list):
                volume = ch_vols[0]
                channels = len(ch_vols)
            mute = bool(p.get("mute", False) or p.get("softMute", False))

        device_id = props.get("device.id")
        if isinstance(device_id, str) and device_id.isdigit():
            device_id = int(device_id)
        elif not isinstance(device_id, int):
            device_id = None

        is_new = obj_id not in self.nodes
        old_volume = None
        old_mute = None

        if not is_new:
            existing = self.nodes[obj_id]
            old_volume = existing.volume
            old_mute = existing.mute
            # Preserve link state across updates
            inlinks = existing.inlinks
            outlinks = existing.outlinks
        else:
            inlinks = set()
            outlinks = set()

        node = PWNode(
            id=obj_id,
            name=primary_name,
            names=names,
            media_class=media_class,
            volume=volume,
            mute=mute,
            channels=channels,
            inlinks=inlinks,
            outlinks=outlinks,
            device_id=device_id,
            props=props,
        )
        self.nodes[obj_id] = node

        # Update name index
        for n in names:
            self.name_index[n] = node

        if is_new:
            await self._emit("audio.node_online", node.to_dict())
        else:
            if old_volume != volume or old_mute != mute:
                await self._emit("audio.volume_changed", {
                    "name": primary_name,
                    "volume": round(volume, 4),
                    "mute": mute,
                })

    async def _handle_link(self, item: dict) -> None:
        """Add or update a link."""
        obj_id = item["id"]
        info = item.get("info", {})
        out_node = info.get("output-node-id")
        in_node = info.get("input-node-id")
        state = info.get("state", "unknown")

        if out_node is None or in_node is None:
            return

        is_new = obj_id not in self.links
        self.links[obj_id] = PWLink(
            id=obj_id,
            output_node_id=out_node,
            input_node_id=in_node,
            state=state,
        )

        # Update node link tracking
        if out_node in self.nodes:
            self.nodes[out_node].outlinks.add(in_node)
        if in_node in self.nodes:
            self.nodes[in_node].inlinks.add(out_node)

        if is_new:
            src_name = self.nodes[out_node].name if out_node in self.nodes else str(out_node)
            snk_name = self.nodes[in_node].name if in_node in self.nodes else str(in_node)
            await self._emit("audio.link_changed", {
                "action": "added",
                "source": src_name,
                "sink": snk_name,
                "state": state,
            })

    async def _handle_delete(self, obj_id: int, obj_type: str) -> None:
        """Handle object deletion."""
        if obj_type == "PipeWire:Interface:Link":
            link = self.links.pop(obj_id, None)
            if link:
                # Remove from node link sets
                if link.output_node_id in self.nodes:
                    self.nodes[link.output_node_id].outlinks.discard(link.input_node_id)
                if link.input_node_id in self.nodes:
                    self.nodes[link.input_node_id].inlinks.discard(link.output_node_id)

                src_name = (self.nodes[link.output_node_id].name
                            if link.output_node_id in self.nodes
                            else str(link.output_node_id))
                snk_name = (self.nodes[link.input_node_id].name
                            if link.input_node_id in self.nodes
                            else str(link.input_node_id))
                await self._emit("audio.link_changed", {
                    "action": "removed",
                    "source": src_name,
                    "sink": snk_name,
                })

        elif obj_type == "PipeWire:Interface:Node":
            node = self.nodes.pop(obj_id, None)
            if node:
                # Clean up name index
                for n in node.names:
                    if self.name_index.get(n) is node:
                        del self.name_index[n]
                # Clean up link refs in other nodes
                for other in self.nodes.values():
                    other.inlinks.discard(obj_id)
                    other.outlinks.discard(obj_id)
                await self._emit("audio.node_offline", {"name": node.name})

    async def _emit(self, event_type: str, data: dict) -> None:
        """Fire callback if set."""
        if self.on_event:
            try:
                await self.on_event(event_type, data)
            except Exception:
                log.exception("PW watcher callback error for %s", event_type)


# ── Subprocess helper ────────────────────────────────────────────────────────

async def _run_pw_cmd(args: list[str], env: dict[str, str] | None = None) -> tuple[int, str]:
    """Run a pw-* command, return (returncode, stderr)."""
    try:
        proc = await asyncio.create_subprocess_exec(
            *args,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
            env=env,
        )
        _, stderr_b = await asyncio.wait_for(proc.communicate(), timeout=5.0)
        return proc.returncode or 0, (stderr_b or b"").decode(errors="replace").strip()
    except asyncio.TimeoutError:
        return -1, "timeout"
    except FileNotFoundError:
        return -1, f"{args[0]} not found"
