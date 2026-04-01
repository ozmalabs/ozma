# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Audio output targets — network and local receivers for ozma audio.

An AudioOutput represents a destination for the active node's audio.
Instead of always routing to the local default PipeWire sink, ozma can
route to any combination of:

  - Local PipeWire sink (default speakers/headphones)
  - AirPlay / RAOP speakers (Sonos, HomePod, HiFiBerry, etc.)
  - RTP / AES67 receivers (professional audio-over-IP)
  - ROC network audio receivers (low-latency with FEC)
  - PulseAudio tunnel sinks (remote PulseAudio/PipeWire instances)
  - Snapcast server (synchronised multi-room audio)
  - VBAN receivers (VB-Audio network, already in ozma for nodes)
  - Bluetooth A2DP sinks (managed by PipeWire natively)

Each output creates a named PipeWire sink via ``pactl load-module`` or
``pw-loopback``.  The AudioRouter links the active node's audio source
to whichever output(s) are selected.

Auto-discovery:
  - AirPlay/RAOP: via mDNS (_raop._tcp)
  - PulseAudio: via mDNS (_pulse-server._tcp, _pulse-sink._tcp)
  - Bluetooth: PipeWire tracks these natively (show up in PipeWireWatcher)
  - Snapcast/ROC/RTP: configured manually
"""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass, field
from typing import Any, Callable, Coroutine

log = logging.getLogger("ozma.audio_outputs")


@dataclass
class AudioOutput:
    """A destination for audio output."""

    id: str                         # Unique identifier
    name: str                       # Human-readable name
    protocol: str                   # "local", "raop", "rtp", "roc", "pulse", "snapcast", "vban", "bluetooth"
    host: str = ""                  # Network host (empty for local)
    port: int = 0                   # Network port
    pw_sink_name: str = ""          # PipeWire sink name (once created)
    pw_module_id: int | None = None # PipeWire module ID (for unloading)
    available: bool = True          # Currently reachable
    enabled: bool = False           # Currently receiving audio (multiple can be enabled)
    delay_ms: float = 0.0          # Delay in milliseconds for time alignment
    props: dict = field(default_factory=dict)  # Protocol-specific properties

    # Managed internally — pw-loopback process for delay, if delay_ms > 0
    _delay_proc: asyncio.subprocess.Process | None = field(
        default=None, repr=False, compare=False
    )

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "protocol": self.protocol,
            "host": self.host,
            "port": self.port,
            "pw_sink_name": self.pw_sink_name,
            "available": self.available,
            "enabled": self.enabled,
            "delay_ms": self.delay_ms,
        }


# Callback type for events
OutputCallback = Callable[[str, dict], Coroutine[Any, Any, None]]


class AudioOutputManager:
    """
    Manages audio output targets with multi-output and time alignment.

    Multiple outputs can be enabled simultaneously.  Each output has a
    configurable delay (in ms) for time-aligning fast local outputs with
    slower network outputs (e.g. AirPlay at ~200ms).

    Delay is implemented via ``pw-loopback --delay <seconds>`` which
    inserts a fixed audio delay between the source and the output sink.

    Usage::

        mgr = AudioOutputManager()
        mgr.on_event = my_callback
        await mgr.start()

        await mgr.enable_output("local")
        await mgr.enable_output("raop-living-room")
        await mgr.set_delay("local", 200)  # delay local by 200ms to match AirPlay

        # AudioRouter calls this to get all enabled sinks
        sinks = mgr.get_enabled_sinks()

        # Or to connect a source to all enabled outputs with delay
        await mgr.connect_source("ozma-vm1")
    """

    def __init__(self) -> None:
        self._outputs: dict[str, AudioOutput] = {}
        self._discovery_task: asyncio.Task | None = None
        self.on_event: OutputCallback | None = None
        self._active_source: str | None = None  # current audio source being routed

        # Always add the local default output
        self._outputs["local"] = AudioOutput(
            id="local",
            name="Local Output",
            protocol="local",
            pw_sink_name="",  # empty = use PipeWire default
            enabled=True,
        )

    async def start(self) -> None:
        """Start output discovery and management."""
        self._discovery_task = asyncio.create_task(
            self._discovery_loop(), name="audio-output-discovery"
        )
        log.info("AudioOutputManager started")

    async def stop(self) -> None:
        if self._discovery_task:
            self._discovery_task.cancel()
            try:
                await self._discovery_task
            except asyncio.CancelledError:
                pass
        # Stop all delay loopbacks
        for output in self._outputs.values():
            await self._stop_delay_loopback(output)
            if output.pw_module_id is not None:
                await _pactl("unload-module", str(output.pw_module_id))

    def list_outputs(self) -> list[dict[str, Any]]:
        return [o.to_dict() for o in self._outputs.values()]

    def get_output(self, output_id: str) -> AudioOutput | None:
        return self._outputs.get(output_id)

    def get_enabled_sinks(self) -> list[str]:
        """Return PW sink names for all enabled outputs (without delay — use connect_source for delay)."""
        sinks = []
        for o in self._outputs.values():
            if o.enabled:
                sinks.append(o.pw_sink_name or "")
        return sinks

    def get_selected_sink(self) -> str | None:
        """Compat: return first enabled sink (used by AudioRouter._get_output_sink)."""
        for o in self._outputs.values():
            if o.enabled:
                return o.pw_sink_name or None
        return None

    # ── Enable / disable outputs ─────────────────────────────────────────────

    async def enable_output(self, output_id: str) -> bool:
        """Enable an output (audio will be routed to it)."""
        output = self._outputs.get(output_id)
        if not output:
            return False

        if output.protocol != "local" and not output.pw_sink_name:
            ok = await self._create_pw_sink(output)
            if not ok:
                return False

        output.enabled = True

        # If there's an active source, connect this output now
        if self._active_source:
            await self._connect_output(self._active_source, output)

        await self._emit("audio.output_changed", output.to_dict())
        log.info("Audio output enabled: %s (%s, delay=%dms)",
                 output.name, output.protocol, output.delay_ms)
        return True

    async def disable_output(self, output_id: str) -> bool:
        """Disable an output (stop routing audio to it)."""
        output = self._outputs.get(output_id)
        if not output:
            return False

        output.enabled = False

        # Disconnect and stop delay loopback
        if self._active_source:
            await self._disconnect_output(self._active_source, output)
        await self._stop_delay_loopback(output)

        await self._emit("audio.output_changed", output.to_dict())
        log.info("Audio output disabled: %s", output.name)
        return True

    # ── Delay control ────────────────────────────────────────────────────────

    async def set_delay(self, output_id: str, delay_ms: float) -> bool:
        """Set time-alignment delay on an output (0 = no delay)."""
        output = self._outputs.get(output_id)
        if not output:
            return False

        old_delay = output.delay_ms
        output.delay_ms = max(0.0, delay_ms)

        # If the output is active and delay changed, reconnect with new delay
        if output.enabled and self._active_source and old_delay != output.delay_ms:
            await self._disconnect_output(self._active_source, output)
            await self._stop_delay_loopback(output)
            await self._connect_output(self._active_source, output)

        await self._emit("audio.output_changed", output.to_dict())
        log.info("Audio output delay: %s = %.1fms", output.name, output.delay_ms)
        return True

    # ── Source connection (called by AudioRouter) ────────────────────────────

    async def connect_source(self, source_name: str) -> None:
        """Connect an audio source to all enabled outputs (with delays)."""
        old_source = self._active_source
        self._active_source = source_name

        # Disconnect old source from all outputs
        if old_source:
            for output in self._outputs.values():
                if output.enabled:
                    await self._disconnect_output(old_source, output)
                    await self._stop_delay_loopback(output)

        # Connect new source to all enabled outputs
        for output in self._outputs.values():
            if output.enabled:
                await self._connect_output(source_name, output)

    async def disconnect_all(self) -> None:
        """Disconnect the current source from all outputs."""
        if self._active_source:
            for output in self._outputs.values():
                if output.enabled:
                    await self._disconnect_output(self._active_source, output)
                    await self._stop_delay_loopback(output)
        self._active_source = None

    async def _connect_output(self, source: str, output: AudioOutput) -> None:
        """Connect a source to one output, applying delay if configured."""
        sink = output.pw_sink_name
        if not sink and output.protocol == "local":
            # Local output uses the PipeWire default sink
            result = await _pactl("get-default-sink")
            sink = result if result else None
        if not sink:
            return

        if output.delay_ms > 0:
            # Use pw-loopback with --delay for time alignment
            delay_sec = output.delay_ms / 1000.0
            loopback_name = f"ozma-delay-{_sanitize(output.id)}"
            proc = await asyncio.create_subprocess_exec(
                "pw-loopback",
                "--name", loopback_name,
                "--capture", source,
                "--playback", sink,
                "--delay", str(delay_sec),
                "--channels", "2",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            output._delay_proc = proc
            log.debug("Delay loopback: %s → %s (%.1fms, pid %d)",
                      source, sink, output.delay_ms, proc.pid)
        else:
            # Direct pw-link, no delay
            rc, err = await _run_cmd(["pw-link", source, sink])
            if rc != 0 and "already linked" not in err:
                log.debug("pw-link %s → %s: %s", source, sink, err)

    async def _disconnect_output(self, source: str, output: AudioOutput) -> None:
        """Disconnect a source from one output."""
        sink = output.pw_sink_name
        if not sink and output.protocol == "local":
            result = await _pactl("get-default-sink")
            sink = result if result else None
        if not sink:
            return

        if output._delay_proc is None:
            # Direct link — unlink it
            await _run_cmd(["pw-link", "--disconnect", source, sink])

    async def _stop_delay_loopback(self, output: AudioOutput) -> None:
        """Stop a delay loopback process."""
        proc = output._delay_proc
        if proc and proc.returncode is None:
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), timeout=2.0)
            except asyncio.TimeoutError:
                proc.kill()
        output._delay_proc = None

    async def add_output(self, output: AudioOutput) -> None:
        """Manually add an output (for configured outputs)."""
        self._outputs[output.id] = output
        await self._emit("audio.output_discovered", output.to_dict())

    # ── PipeWire sink creation ───────────────────────────────────────────────

    async def _create_pw_sink(self, output: AudioOutput) -> bool:
        """Create a PipeWire sink for a network output."""
        match output.protocol:
            case "raop":
                return await self._create_raop_sink(output)
            case "rtp":
                return await self._create_rtp_sink(output)
            case "roc":
                return await self._create_roc_sink(output)
            case "pulse":
                return await self._create_pulse_tunnel(output)
            case "snapcast":
                return await self._create_snapcast_sink(output)
            case "vban":
                return await self._create_vban_sink(output)
            case _:
                log.warning("Unknown output protocol: %s", output.protocol)
                return False

    async def _create_raop_sink(self, output: AudioOutput) -> bool:
        """Create an AirPlay/RAOP sink via PipeWire module."""
        sink_name = f"ozma-raop-{_sanitize(output.id)}"
        # PipeWire's RAOP module: loaded via pactl/pw-cli
        # We use pactl load-module which creates a tunnel sink
        module_id = await _pactl(
            "load-module", "module-raop-sink",
            f"server=[{output.host}]:{output.port}",
            f"sink_name={sink_name}",
            f"sink_properties=device.description=\"{output.name}\"",
        )
        if module_id:
            output.pw_sink_name = sink_name
            output.pw_module_id = int(module_id) if module_id.isdigit() else None
            return True
        # Fallback: use PipeWire native module loading
        return await self._create_pw_module_sink(output, sink_name, {
            "library.name": "libpipewire-module-raop-sink",
            "args": {
                "raop.ip": output.host,
                "raop.port": output.port,
                "raop.transport": output.props.get("transport", "udp"),
                "node.name": sink_name,
                "node.description": output.name,
            }
        })

    async def _create_rtp_sink(self, output: AudioOutput) -> bool:
        """Create an RTP/AES67 sink."""
        sink_name = f"ozma-rtp-{_sanitize(output.id)}"
        dest = output.host or "224.0.0.56"  # default multicast group
        port = output.port or 46000
        return await self._create_pw_module_sink(output, sink_name, {
            "library.name": "libpipewire-module-rtp-sink",
            "args": {
                "destination.ip": dest,
                "destination.port": port,
                "node.name": sink_name,
                "node.description": output.name,
                "sess.name": f"ozma-{output.id}",
            }
        })

    async def _create_roc_sink(self, output: AudioOutput) -> bool:
        """Create a ROC network audio sink."""
        sink_name = f"ozma-roc-{_sanitize(output.id)}"
        return await self._create_pw_module_sink(output, sink_name, {
            "library.name": "libpipewire-module-roc-sink",
            "args": {
                "remote.ip": output.host,
                "remote.source.port": output.props.get("source_port", 10001),
                "remote.repair.port": output.props.get("repair_port", 10002),
                "fec.code": output.props.get("fec", "rs8m"),
                "node.name": sink_name,
                "node.description": output.name,
            }
        })

    async def _create_pulse_tunnel(self, output: AudioOutput) -> bool:
        """Create a PulseAudio tunnel sink to a remote PA/PW server."""
        sink_name = f"ozma-pulse-{_sanitize(output.id)}"
        server = f"{output.host}:{output.port}" if output.port else output.host
        module_id = await _pactl(
            "load-module", "module-tunnel-sink",
            f"server={server}",
            f"sink_name={sink_name}",
            f"sink_properties=device.description=\"{output.name}\"",
        )
        if module_id:
            output.pw_sink_name = sink_name
            output.pw_module_id = int(module_id) if module_id.isdigit() else None
            return True
        return False

    async def _create_snapcast_sink(self, output: AudioOutput) -> bool:
        """Create a pipe sink for Snapcast (feeds snapserver via named pipe or TCP)."""
        sink_name = f"ozma-snapcast-{_sanitize(output.id)}"
        pipe_path = output.props.get("pipe", "/tmp/ozma-snapcast")
        # Create a pipe sink that Snapcast's snapserver reads from
        module_id = await _pactl(
            "load-module", "module-pipe-sink",
            f"file={pipe_path}",
            f"sink_name={sink_name}",
            f"sink_properties=device.description=\"{output.name}\"",
            "format=s16le", "rate=48000", "channels=2",
        )
        if module_id:
            output.pw_sink_name = sink_name
            output.pw_module_id = int(module_id) if module_id.isdigit() else None
            return True
        return False

    async def _create_vban_sink(self, output: AudioOutput) -> bool:
        """Create a VBAN sender sink."""
        sink_name = f"ozma-vban-{_sanitize(output.id)}"
        return await self._create_pw_module_sink(output, sink_name, {
            "library.name": "libpipewire-module-vban-send",
            "args": {
                "destination.ip": output.host,
                "destination.port": output.port or 6980,
                "node.name": sink_name,
                "node.description": output.name,
                "sess.name": f"ozma-{output.id}",
            }
        })

    async def _create_pw_module_sink(
        self, output: AudioOutput, sink_name: str, module_config: dict
    ) -> bool:
        """Load a PipeWire module to create a sink."""
        args_json = json.dumps(module_config.get("args", {}))
        lib = module_config.get("library.name", "")
        rc, err = await _run_cmd(
            ["pw-cli", "create-object", lib, args_json]
        )
        if rc == 0:
            output.pw_sink_name = sink_name
            log.debug("Created PW module sink: %s via %s", sink_name, lib)
            return True
        log.debug("pw-cli create-object %s failed: %s", lib, err)
        return False

    # ── Auto-discovery ───────────────────────────────────────────────────────

    async def _discovery_loop(self) -> None:
        """Periodically discover network audio receivers."""
        while True:
            try:
                await self._discover_raop()
                await self._discover_pulse()
                await asyncio.sleep(30.0)
            except asyncio.CancelledError:
                return
            except Exception:
                log.debug("Output discovery error", exc_info=True)
                await asyncio.sleep(60.0)

    async def _discover_raop(self) -> None:
        """Discover AirPlay/RAOP speakers via avahi-browse."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "avahi-browse", "-t", "-r", "-p", "_raop._tcp",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10.0)
        except (FileNotFoundError, asyncio.TimeoutError):
            return

        for line in stdout.decode(errors="replace").splitlines():
            if not line.startswith("="):
                continue
            parts = line.split(";")
            if len(parts) < 9:
                continue
            # Format: =;iface;proto;name;type;domain;hostname;address;port;txt
            raw_name = parts[3]
            host = parts[7]
            port = int(parts[8]) if parts[8].isdigit() else 0

            # Clean up RAOP name (format: "MACADDR@Device Name")
            # avahi-browse -p escapes special chars as \NNN octal
            display_name = _avahi_unescape(raw_name)
            if "@" in display_name:
                display_name = display_name.split("@", 1)[1]

            output_id = f"raop-{_sanitize(display_name)}"
            if output_id not in self._outputs:
                output = AudioOutput(
                    id=output_id,
                    name=display_name,
                    protocol="raop",
                    host=host,
                    port=port,
                )
                self._outputs[output_id] = output
                await self._emit("audio.output_discovered", output.to_dict())
                log.info("Discovered AirPlay receiver: %s (%s:%d)", display_name, host, port)

    async def _discover_pulse(self) -> None:
        """Discover PulseAudio/PipeWire servers via avahi-browse."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "avahi-browse", "-t", "-r", "-p", "_pulse-sink._tcp",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10.0)
        except (FileNotFoundError, asyncio.TimeoutError):
            return

        for line in stdout.decode(errors="replace").splitlines():
            if not line.startswith("="):
                continue
            parts = line.split(";")
            if len(parts) < 9:
                continue
            name = _avahi_unescape(parts[3])
            host = parts[7]
            port = int(parts[8]) if parts[8].isdigit() else 4713

            output_id = f"pulse-{_sanitize(name)}"
            if output_id not in self._outputs:
                output = AudioOutput(
                    id=output_id,
                    name=f"{name} (PulseAudio)",
                    protocol="pulse",
                    host=host,
                    port=port,
                )
                self._outputs[output_id] = output
                await self._emit("audio.output_discovered", output.to_dict())
                log.info("Discovered PulseAudio sink: %s (%s:%d)", name, host, port)

    async def _emit(self, event_type: str, data: dict) -> None:
        if self.on_event:
            try:
                await self.on_event(event_type, data)
            except Exception:
                log.debug("Output event callback error", exc_info=True)


# ── Helpers ──────────────────────────────────────────────────────────────────

def _avahi_unescape(text: str) -> str:
    """Unescape avahi-browse's \\NNN decimal escape sequences."""
    import re
    def _repl(m: re.Match) -> str:
        return chr(int(m.group(1)))
    return re.sub(r"\\(\d{3})", _repl, text)


def _sanitize(name: str) -> str:
    """Sanitize a name for use as a PipeWire node name."""
    return "".join(c if c.isalnum() or c in "-_" else "-" for c in name).strip("-").lower()


async def _pactl(*args: str) -> str | None:
    """Run a pactl command, return stdout on success or None on failure."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "pactl", *args,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=5.0)
        if proc.returncode == 0:
            return stdout.decode().strip()
        log.debug("pactl %s failed: %s", " ".join(args), stderr.decode().strip())
        return None
    except (FileNotFoundError, asyncio.TimeoutError):
        return None


async def _run_cmd(args: list[str]) -> tuple[int, str]:
    """Run a command, return (returncode, stderr)."""
    try:
        proc = await asyncio.create_subprocess_exec(
            *args,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await asyncio.wait_for(proc.communicate(), timeout=5.0)
        return proc.returncode or 0, (stderr or b"").decode(errors="replace").strip()
    except asyncio.TimeoutError:
        return -1, "timeout"
    except FileNotFoundError:
        return -1, f"{args[0]} not found"
