# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
V0.3 Audio Router — PipeWire-based audio follows active scenario.

Two audio source types are supported:

  pipewire  — soft nodes (QEMU VMs on the same host).
              QEMU outputs to a named PipeWire null sink (e.g. "ozma-vm1").
              The null sink's monitor source is linked to the output sink
              via pw-link when the node is active.

  vban      — hardware nodes (SBCs on the network).
              Node runs a VBAN emitter; controller runs a VBANReceiver per
              node which pipes PCM into pw-cat, creating a named PipeWire
              playback stream.  Same pw-link routing applies.

Mic routing (controller microphone → active node):
  pipewire  — WirePlumber automatically routes the default mic to QEMU
              audio capture clients; no explicit linking needed.
  vban      — VBANSender reads from the default mic and sends VBAN UDP to
              the node's mic listener port.

NodeInfo audio fields (populated from mDNS TXT or direct registration):
  audio_type      "pipewire" | "vban" | None
  audio_sink      PipeWire null sink name (pipewire nodes)
  audio_vban_port UDP port for VBAN emission from node (vban nodes)
  mic_vban_port   UDP port node listens on for incoming mic VBAN (vban nodes)

On scenario switch:
  Default mode (pw-link):
    1. Disconnect old node's audio source from output (pw-link --disconnect).
    2. Connect new node's audio source to output.

  WirePlumber mode (OZMA_AUDIO_WIREPLUMBER=1):
    1. Write pw-metadata -n ozma set 0 active_node <sink_name>
    WirePlumber ozma-routing.lua handles the actual link management.
    See controller/wireplumber/install.sh to set up.
"""

from __future__ import annotations

import asyncio
import logging
import os
import shutil
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from state import AppState, NodeInfo

from vban import VBANReceiver, VBANSender, DEFAULT_PORT
from pipewire_watcher import PipeWireWatcher
from audio_outputs import AudioOutputManager

log = logging.getLogger("ozma.audio")

# pw-link returns only after the link is committed to the PipeWire graph.
_LINK_SETTLE = 0.0

# WirePlumber mode: set OZMA_AUDIO_WIREPLUMBER=1 after installing
# controller/wireplumber/install.sh.  In this mode the controller writes
# a single pw-metadata call per switch; the WP Lua script manages links.
_WP_MODE = os.environ.get("OZMA_AUDIO_WIREPLUMBER", "").lower() in ("1", "true", "yes")


class AudioRouter:
    """
    Manages per-node audio sources and routes the active node's audio
    to the operator output using PipeWire.

    Two routing modes:
      pw-link (default): direct pw-link subprocess calls.
      WirePlumber (OZMA_AUDIO_WIREPLUMBER=1): single pw-metadata call;
        ozma-routing.lua inside WirePlumber manages link lifecycle.
    """

    def __init__(
        self,
        state: "AppState",
        output_sink: str | None = None,     # None = use PipeWire default
        mic_source: str | None = None,      # None = use PipeWire default mic
        enabled: bool = True,
        wireplumber_mode: bool = _WP_MODE,
        alias_map: dict[str, str] | None = None,
    ) -> None:
        self._state = state
        self._output_sink = output_sink
        self._mic_source = mic_source
        self._enabled = enabled
        self._wp_mode = wireplumber_mode
        self._active_node_id: str | None = None

        # VBAN receivers: node_id → VBANReceiver
        self._vban_rx: dict[str, VBANReceiver] = {}
        # VBAN mic senders: node_id → VBANSender
        self._vban_tx: dict[str, VBANSender] = {}

        self._task: asyncio.Task | None = None
        self._switch_lock = asyncio.Lock()  # serialise on_scenario_activated calls
        self._pw_available = (
            shutil.which("pw-metadata") is not None if wireplumber_mode
            else shutil.which("pw-link") is not None
        )

        # Real-time PipeWire graph watcher
        self.watcher = PipeWireWatcher(alias_map=alias_map or {})

        # Audio output targets (local, AirPlay, RTP, ROC, etc.)
        self.outputs = AudioOutputManager()

    async def start(self) -> None:
        if not self._enabled:
            return
        if not self._pw_available:
            tool = "pw-metadata" if self._wp_mode else "pw-link"
            log.warning("%s not found — audio routing disabled", tool)
            self._enabled = False
            return
        if self._wp_mode:
            log.info("AudioRouter: WirePlumber mode (pw-metadata → ozma-routing.lua)")

        # Start PipeWire graph watcher — fires events when nodes appear/change
        self.watcher.on_event = self._on_pw_event
        await self.watcher.start()

        # Start audio output manager (discovers AirPlay, PulseAudio, etc.)
        self.outputs.on_event = self._on_output_event
        await self.outputs.start()

        # Start VBAN receivers for any nodes already online with vban audio
        for node in self._state.nodes.values():
            await self._on_node_online(node)

        # Fallback poll for VBAN nodes (not tracked by PW watcher)
        self._task = asyncio.create_task(self._vban_poll_loop(), name="audio-vban-poll")
        log.info("AudioRouter started")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
        await self.outputs.stop()
        await self.watcher.stop()
        for rx in list(self._vban_rx.values()):
            await rx.stop()
        for tx in list(self._vban_tx.values()):
            await tx.stop()

    async def on_scenario_activated(self, node_id: str | None) -> None:
        """Called by ScenarioManager when a scenario becomes active."""
        if not self._enabled:
            return
        async with self._switch_lock:
            prev = self._active_node_id
            self._active_node_id = node_id
            if prev == node_id:
                return

            prev_node = self._state.nodes.get(prev) if prev else None
            new_node  = self._state.nodes.get(node_id) if node_id else None

            # --- Disconnect previous node ---
            if prev_node:
                await self._disconnect_audio(prev_node)
                await self._disconnect_mic(prev_node)

            # --- Connect new node ---
            if new_node:
                await self._connect_audio(new_node)
                await self._connect_mic(new_node)

            log.info("Audio switched: %s → %s", prev or "none", node_id or "none")

    # ── Volume / mute passthrough ────────────────────────────────────────────

    async def set_volume(self, node_name: str, volume: float) -> bool:
        """Set PipeWire volume on a node (linear 0.0-1.0+)."""
        return await self.watcher.set_volume(node_name, volume)

    async def set_mute(self, node_name: str, mute: bool) -> bool:
        """Set PipeWire mute on a node."""
        return await self.watcher.set_mute(node_name, mute)

    # ── internal ─────────────────────────────────────────────────────────────

    async def _on_output_event(self, event_type: str, data: dict) -> None:
        """Forward audio output events to WebSocket clients."""
        await self._state.events.put({"type": event_type, **data})

    async def _on_pw_event(self, event_type: str, data: dict) -> None:
        """
        Reactive callback from PipeWireWatcher — replaces the old 2s poll.

        When an ozma-relevant PW node appears, we auto-connect audio if it's
        the active node (handles the startup race condition).
        """
        # Forward all PW events to WebSocket clients
        await self._state.events.put({"type": event_type, **data})

        if event_type == "audio.node_online":
            node_name = data.get("name", "")
            # Check if this new PW node is the audio source for the active ozma node
            active_node = self._state.nodes.get(self._active_node_id) if self._active_node_id else None
            if active_node:
                src = self._audio_source_name(active_node)
                if src and node_name == src:
                    log.info("Active node's PW source %s appeared — connecting audio", src)
                    async with self._switch_lock:
                        await self._connect_audio(active_node)

    async def _vban_poll_loop(self) -> None:
        """
        Poll for new VBAN nodes (not tracked by PW watcher since they
        appear as AppState nodes, not PipeWire nodes, until their
        VBANReceiver creates a pw-cat source).
        """
        while True:
            try:
                await asyncio.sleep(2.0)
                for node in list(self._state.nodes.values()):
                    if (node.audio_type == "vban"
                            and node.id not in self._vban_rx
                            and node.audio_vban_port):
                        await self._on_node_online(node)
            except asyncio.CancelledError:
                return

    async def _on_node_online(self, node: "NodeInfo") -> None:
        if not self._enabled:
            return
        if node.audio_type == "vban" and node.audio_vban_port:
            if node.id not in self._vban_rx:
                rx = VBANReceiver(
                    bind_port=node.audio_vban_port,
                    stream_name=f"ozma-{node.id.split('.')[0]}",
                )
                self._vban_rx[node.id] = rx
                await rx.start()
                log.info("VBAN receiver started for %s on port %d",
                         node.id, node.audio_vban_port)

    async def _on_node_offline(self, node_id: str) -> None:
        if node_id == self._active_node_id:
            self._active_node_id = None
        rx = self._vban_rx.pop(node_id, None)
        if rx:
            await rx.stop()
        tx = self._vban_tx.pop(node_id, None)
        if tx:
            await tx.stop()

    # ── PipeWire link helpers ─────────────────────────────────────────────────

    def _audio_source_name(self, node: "NodeInfo") -> str | None:
        """PipeWire source (output) node name for this node."""
        if node.audio_type == "pipewire" and node.audio_sink:
            # pw-link resolves by node name; the null sink node "ozma-vm1"
            # exposes monitor_FL/FR output ports that pw-link matches automatically.
            return node.audio_sink
        if node.audio_type == "vban" and node.id in self._vban_rx:
            return self._vban_rx[node.id].stream_name
        return None

    def _mic_sink_name(self, node: "NodeInfo") -> str | None:
        """PipeWire sink (input) node name for sending mic to this node."""
        if node.audio_type == "pipewire":
            # QEMU's PipeWire client node is named after the QEMU -name flag,
            # which matches the first label of the mDNS instance name.
            # e.g. "vm1._ozma._udp.local." → PW node "vm1"
            return node.id.split(".")[0]
        return None

    async def _connect_audio(self, node: "NodeInfo") -> None:
        src = self._audio_source_name(node)
        if not src:
            return
        if self._wp_mode:
            await _pw_meta_set_active(src)
            return
        # Route to all enabled outputs (with per-output delay)
        await self.outputs.connect_source(src)

    async def _disconnect_audio(self, node: "NodeInfo") -> None:
        src = self._audio_source_name(node)
        if not src:
            return
        if self._wp_mode:
            # WirePlumber clears links when active_node is set to "" on next switch.
            # Explicit disconnect not needed; avoid race with connect_audio.
            return
        await self.outputs.disconnect_all()

    async def _connect_mic(self, node: "NodeInfo") -> None:
        if node.audio_type == "vban" and node.mic_vban_port and node.host:
            # Start VBAN sender: controller mic → node
            if node.id not in self._vban_tx:
                tx = VBANSender(
                    target_host=node.host,
                    target_port=node.mic_vban_port,
                    source_name=self._mic_source,
                    stream_name="ozma-mic",
                )
                self._vban_tx[node.id] = tx
                await tx.start()
                log.info("VBAN mic sender started → %s:%d", node.host, node.mic_vban_port)
        # For pipewire soft nodes: WirePlumber (PipeWire session manager) automatically
        # routes the default mic source to all active QEMU audio capture clients.
        # Explicit pw-link calls here fight against WirePlumber's auto-routing and are
        # unreliable. Let WirePlumber handle mic routing for soft nodes.

    async def _disconnect_mic(self, node: "NodeInfo") -> None:
        if node.audio_type == "vban":
            tx = self._vban_tx.pop(node.id, None)
            if tx:
                await tx.stop()
        # pipewire: WirePlumber manages mic routing automatically; no explicit disconnect.

    async def _get_output_sink(self) -> str | None:
        # Check if an audio output target is selected (AirPlay, RTP, etc.)
        selected = self.outputs.get_selected_sink()
        if selected:
            return selected
        if self._output_sink:
            return self._output_sink
        return await _get_default_sink()

    async def _get_mic_source(self) -> str | None:
        if self._mic_source:
            return self._mic_source
        return await _get_default_source()


# ── PipeWire helpers ──────────────────────────────────────────────────────────

async def _run_pw(args: list[str]) -> tuple[int, str]:
    """Run a pw-* command, return (returncode, stderr)."""
    try:
        proc = await asyncio.create_subprocess_exec(
            *args,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr_b = await asyncio.wait_for(proc.communicate(), timeout=5.0)
        return proc.returncode or 0, (stderr_b or b"").decode(errors="replace").strip()
    except asyncio.TimeoutError:
        return -1, "timeout"
    except FileNotFoundError:
        return -1, f"{args[0]} not found"


async def _pw_meta_set_active(source_name: str) -> None:
    """
    WirePlumber mode: write active_node to the ozma metadata namespace.
    ozma-routing.lua watches this and manages the actual PipeWire links.
    """
    rc, err = await _run_pw(
        ["pw-metadata", "-n", "ozma", "set", "0", "active_node", source_name]
    )
    if rc != 0:
        log.warning("pw-metadata set active_node=%s failed (rc=%d): %s", source_name, rc, err)
    else:
        log.debug("pw-metadata: active_node → %s", source_name or "(none)")


async def _pw_link(source: str, sink: str) -> None:
    """Link all channels of a PipeWire source to a sink."""
    rc, err = await _run_pw(["pw-link", source, sink])
    if rc != 0 and "already linked" not in err and "not found" not in err.lower():
        log.debug("pw-link %s → %s: %s (rc=%d)", source, sink, err, rc)
    else:
        log.debug("pw-link: %s → %s", source, sink)
    await asyncio.sleep(_LINK_SETTLE)


async def _pw_unlink(source: str, sink: str) -> None:
    """Unlink all channels of a PipeWire source from a sink."""
    rc, err = await _run_pw(["pw-link", "--disconnect", source, sink])
    if rc != 0 and "not linked" not in err and "not found" not in err.lower():
        log.debug("pw-link --disconnect %s → %s: %s (rc=%d)", source, sink, err, rc)
    await asyncio.sleep(_LINK_SETTLE)


async def _get_default_sink() -> str | None:
    """Return the name of the default PipeWire output sink via pactl."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "pactl", "get-default-sink",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await asyncio.wait_for(proc.communicate(), timeout=3.0)
        name = out.decode().strip()
        return name or None
    except Exception:
        return None


async def _get_default_source() -> str | None:
    """Return the name of the default PipeWire input source via pactl."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "pactl", "get-default-source",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await asyncio.wait_for(proc.communicate(), timeout=3.0)
        name = out.decode().strip()
        return name or None
    except Exception:
        return None


async def create_null_sink(sink_name: str, description: str | None = None) -> bool:
    """
    Create a PipeWire/PulseAudio null sink with the given name.
    Returns True on success.  Safe to call if the sink already exists.
    """
    desc = description or sink_name
    try:
        proc = await asyncio.create_subprocess_exec(
            "pactl", "load-module", "module-null-sink",
            f"sink_name={sink_name}",
            f"sink_properties=device.description={desc}",
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _, err = await asyncio.wait_for(proc.communicate(), timeout=5.0)
        err_str = (err or b"").decode(errors="replace").strip()
        if proc.returncode == 0:
            log.debug("Created null sink: %s", sink_name)
            return True
        if "already exists" in err_str or "exists" in err_str:
            return True   # idempotent
        log.warning("Failed to create null sink '%s': %s", sink_name, err_str)
        return False
    except Exception as e:
        log.warning("create_null_sink '%s': %s", sink_name, e)
        return False
