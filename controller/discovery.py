# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
mDNS listener for _ozma._udp.local. service announcements.

Each node advertises itself with these TXT records:
  proto=<int>           protocol version (must be 1)
  role=<str>            node role: compute | presence | room-mic | display | ...
  hw=<str>              hardware type: milkv-duos | rpi-zero2w | teensy41 | soft | ...
  fw=<str>              firmware version string
  cap=<csv>             comma-separated capability list (optional)
  machine_class=<str>   workstation | server | kiosk | camera (default: workstation)
  frigate_host=<str>    Frigate HTTP host (camera nodes only)
  frigate_port=<int>    Frigate HTTP port (camera nodes only, default: 5000)
  camera_streams=<json> JSON array of stream dicts (camera nodes only)

Re-queries every mdns_requery_interval seconds to detect node loss.
"""

import asyncio
import json
import logging
import socket
import time
from collections.abc import Awaitable, Callable
from typing import TYPE_CHECKING

from zeroconf import ServiceBrowser, ServiceStateChange, Zeroconf
from zeroconf.asyncio import AsyncServiceInfo, AsyncZeroconf

from state import AppState, NodeInfo
from config import Config

if TYPE_CHECKING:
    pass

log = logging.getLogger("ozma.discovery")

REQUIRED_PROTO_VERSION = 1


def _parse_txt(properties: dict[bytes, bytes | None]) -> dict[str, str]:
    out: dict[str, str] = {}
    for k, v in properties.items():
        key = k.decode("utf-8", errors="replace")
        val = v.decode("utf-8", errors="replace") if v is not None else ""
        out[key] = val
    return out


class DiscoveryService:
    def __init__(self, config: Config, state: AppState) -> None:
        self._config = config
        self._state = state
        self._azc: AsyncZeroconf | None = None
        self._browser: ServiceBrowser | None = None
        self._requery_task: asyncio.Task | None = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._peer_browser: object | None = None  # AsyncServiceBrowser for ctrl peers
        self._on_peer_found: Callable[[dict], Awaitable[None]] | None = None
        self._on_peer_lost: Callable[[str], Awaitable[None]] | None = None

    async def start(self) -> None:
        self._loop = asyncio.get_running_loop()
        self._azc = AsyncZeroconf()
        self._browser = ServiceBrowser(
            self._azc.zeroconf,
            self._config.mdns_service_type,
            handlers=[self._on_service_state_change],
        )
        log.info("mDNS browser started for %s", self._config.mdns_service_type)
        self._requery_task = asyncio.create_task(self._requery_loop(), name="mdns-requery")

    async def stop(self) -> None:
        if self._requery_task:
            self._requery_task.cancel()
        if self._azc:
            await self._azc.async_close()

    def _on_service_state_change(
        self,
        zeroconf: Zeroconf,
        service_type: str,
        name: str,
        state_change: ServiceStateChange,
    ) -> None:
        # Called from a zeroconf thread — schedule onto the asyncio event loop
        assert self._loop is not None
        if state_change in (ServiceStateChange.Added, ServiceStateChange.Updated):
            self._loop.call_soon_threadsafe(
                self._loop.create_task,
                self._resolve_and_add(zeroconf, service_type, name),
            )
        elif state_change == ServiceStateChange.Removed:
            self._loop.call_soon_threadsafe(
                self._loop.create_task,
                self._state.remove_node(name),
            )

    async def _resolve_and_add(
        self, zeroconf: Zeroconf, service_type: str, name: str
    ) -> None:
        info = AsyncServiceInfo(service_type, name)
        await info.async_request(zeroconf, timeout=3000)

        addresses = info.parsed_addresses()
        if not addresses:
            log.warning("Could not resolve address for %s", name)
            return

        host = addresses[0]
        port = info.port or self._config.node_port
        txt = _parse_txt(info.properties)

        proto = int(txt.get("proto", "0"))
        if proto != REQUIRED_PROTO_VERSION:
            log.warning(
                "Node %s advertises proto=%d, expected %d — ignoring",
                name, proto, REQUIRED_PROTO_VERSION,
            )
            return

        caps_raw = txt.get("cap", "")
        capabilities = [c.strip() for c in caps_raw.split(",") if c.strip()]

        vnc_port_str = txt.get("vnc_port", "")
        vnc_port = int(vnc_port_str) if vnc_port_str.isdigit() else None
        vnc_host = txt.get("vnc_host") or None

        stream_port_str = txt.get("stream_port", "")
        stream_port = int(stream_port_str) if stream_port_str.isdigit() else None
        stream_path = txt.get("stream_path") or None

        api_port_str = txt.get("api_port", "")
        api_port = int(api_port_str) if api_port_str.isdigit() else stream_port

        audio_type = txt.get("audio_type") or None
        audio_sink = txt.get("audio_sink") or None
        audio_vban_str = txt.get("audio_vban_port", "")
        audio_vban_port = int(audio_vban_str) if audio_vban_str.isdigit() else None
        mic_vban_str = txt.get("mic_vban_port", "")
        mic_vban_port = int(mic_vban_str) if mic_vban_str.isdigit() else None
        capture_device = txt.get("capture_device") or None

        machine_class = txt.get("machine_class") or "workstation"
        frigate_host = txt.get("frigate_host") or None
        frigate_port_str = txt.get("frigate_port", "")
        frigate_port = int(frigate_port_str) if frigate_port_str.isdigit() else None
        camera_streams_raw = txt.get("camera_streams", "")
        try:
            camera_streams = json.loads(camera_streams_raw) if camera_streams_raw else []
        except json.JSONDecodeError:
            log.warning("Node %s has malformed camera_streams TXT record", name)
            camera_streams = []

        node = NodeInfo(
            id=name,
            host=host,
            port=port,
            role=txt.get("role", "unknown"),
            hw=txt.get("hw", "unknown"),
            fw_version=txt.get("fw", "unknown"),
            proto_version=proto,
            capabilities=capabilities,
            last_seen=time.monotonic(),
            vnc_host=vnc_host,
            vnc_port=vnc_port,
            stream_port=stream_port,
            stream_path=stream_path,
            api_port=api_port,
            audio_type=audio_type,
            audio_sink=audio_sink,
            audio_vban_port=audio_vban_port,
            mic_vban_port=mic_vban_port,
            capture_device=capture_device,
            machine_class=machine_class,
            frigate_host=frigate_host,
            frigate_port=frigate_port,
            camera_streams=camera_streams,
        )
        await self._state.add_node(node)
        log.info("Node online: %s @ %s:%d role=%s hw=%s", name, host, port, node.role, node.hw)

    # ── Controller advertisement ──────────────────────────────────────────

    def _get_local_ip(self) -> str:
        """Return the primary local IP address."""
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            return s.getsockname()[0]
        except Exception:
            return "127.0.0.1"
        finally:
            s.close()

    async def announce_controller(self, controller_id: str, api_port: int) -> None:
        """Advertise this controller as _ozma-ctrl._tcp.local."""
        if self._azc is None:
            return
        from zeroconf.asyncio import AsyncServiceInfo as _ASI
        info = _ASI(
            "_ozma-ctrl._tcp.local.",
            f"{controller_id}._ozma-ctrl._tcp.local.",
            addresses=[socket.inet_aton(self._get_local_ip())],
            port=api_port,
            properties={
                b"api_port": str(api_port).encode(),
                b"controller_id": controller_id.encode(),
                b"version": b"1",
            },
            server=f"{controller_id}.local.",
        )
        await self._azc.async_register_service(info)
        self._ctrl_info = info
        log.info("Controller advertised as %s._ozma-ctrl._tcp.local.", controller_id)

    async def withdraw_controller(self) -> None:
        """Withdraw the controller's mDNS advertisement."""
        info = getattr(self, "_ctrl_info", None)
        if info and self._azc:
            try:
                await self._azc.async_unregister_service(info)
            except Exception as e:
                log.debug("withdraw_controller: %s", e)
            self._ctrl_info = None

    async def start_peer_browser(
        self,
        on_found: Callable[[dict], Awaitable[None]],
        on_lost: Callable[[str], Awaitable[None]],
    ) -> None:
        """Start a persistent background browser for _ozma-ctrl._tcp.local. peers.

        ``on_found`` is called with a dict: {id, host, api_port, base_url}
        when a peer is first seen or its address changes.

        ``on_lost`` is called with controller_id when a peer goes away.
        """
        if self._azc is None:
            return
        from zeroconf import ServiceStateChange as SSC
        from zeroconf.asyncio import AsyncServiceBrowser as _ASB

        self._on_peer_found = on_found
        self._on_peer_lost = on_lost
        assert self._loop is not None

        def _on_ctrl_state_change(
            zeroconf: Zeroconf,
            service_type: str,
            name: str,
            state_change: SSC,
        ) -> None:
            if state_change in (SSC.Added, SSC.Updated):
                self._loop.call_soon_threadsafe(  # type: ignore[union-attr]
                    self._loop.create_task,  # type: ignore[union-attr]
                    self._resolve_peer(zeroconf, name),
                )
            elif state_change == SSC.Removed:
                ctrl_id = name.split(".")[0]
                self._loop.call_soon_threadsafe(  # type: ignore[union-attr]
                    self._loop.create_task,  # type: ignore[union-attr]
                    self._peer_lost(ctrl_id),
                )

        self._peer_browser = _ASB(
            self._azc.zeroconf,
            "_ozma-ctrl._tcp.local.",
            handlers=[_on_ctrl_state_change],
        )
        log.info("Peer controller browser started")

    async def _resolve_peer(self, zeroconf: Zeroconf, name: str) -> None:
        """Resolve a peer controller service record and fire on_found."""
        from zeroconf.asyncio import AsyncServiceInfo as _ASI
        info = _ASI("_ozma-ctrl._tcp.local.", name)
        await info.async_request(zeroconf, timeout=3000)
        if not info.addresses:
            log.warning("Could not resolve address for peer %s", name)
            return
        ip = socket.inet_ntoa(info.addresses[0])
        props = {
            k.decode(): (v.decode() if isinstance(v, bytes) else (v or ""))
            for k, v in (info.properties or {}).items()
        }
        api_port = int(props.get("api_port", "7380"))
        ctrl_id = props.get("controller_id", name.split(".")[0])

        # Skip self
        my_info = getattr(self, "_ctrl_info", None)
        if my_info and name == my_info.name:
            return

        log.info("Peer controller seen: %s @ %s:%d", ctrl_id, ip, api_port)
        if self._on_peer_found:
            await self._on_peer_found({
                "id": ctrl_id,
                "host": ip,
                "api_port": api_port,
                "base_url": f"http://{ip}:{api_port}",
            })

    async def _peer_lost(self, ctrl_id: str) -> None:
        """Fire on_lost for a peer that has gone offline."""
        log.info("Peer controller lost: %s", ctrl_id)
        if self._on_peer_lost:
            await self._on_peer_lost(ctrl_id)

    async def discover_controllers(self, timeout: float = 5.0) -> list[dict]:
        """Probe mDNS for _ozma-ctrl._tcp.local. peers on the LAN."""
        if self._azc is None:
            return []
        from zeroconf import ServiceStateChange as SSC
        from zeroconf.asyncio import AsyncServiceInfo as _ASI, AsyncServiceBrowser as _ASB
        found: list[dict] = []
        my_id = getattr(getattr(self, "_ctrl_info", None), "name", None)

        async def _resolve(name: str) -> None:
            info = _ASI("_ozma-ctrl._tcp.local.", name)
            await info.async_request(self._azc.zeroconf, timeout=3000)  # type: ignore[union-attr]
            if not info.addresses:
                return
            ip = socket.inet_ntoa(info.addresses[0])
            props = {
                k.decode(): v.decode() if isinstance(v, bytes) else (v or "")
                for k, v in (info.properties or {}).items()
            }
            api_port = int(props.get("api_port", "7380"))
            ctrl_id = props.get("controller_id", name.split(".")[0])
            # Skip self
            if my_id and name == my_id:
                return
            found.append({"id": ctrl_id, "host": ip, "api_port": api_port,
                          "base_url": f"http://{ip}:{api_port}"})

        tasks: list[asyncio.Task] = []

        def _on_change(zc, stype, name, state_change):
            if state_change == SSC.Added:
                tasks.append(asyncio.get_event_loop().create_task(_resolve(name)))

        browser = _ASB(self._azc.zeroconf, "_ozma-ctrl._tcp.local.", handlers=[_on_change])
        await asyncio.sleep(timeout)
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
        browser.cancel()
        return found

    # ── Requery loop ──────────────────────────────────────────────────────

    async def _requery_loop(self) -> None:
        interval = self._config.mdns_requery_interval
        while True:
            await asyncio.sleep(interval)
            await self._requery_all()

    async def _requery_all(self) -> None:
        now = time.monotonic()
        stale_threshold = self._config.mdns_requery_interval * 2

        for nid, node in list(self._state.nodes.items()):
            if node.direct_registered:
                # Direct-registered nodes stay alive as long as they keep
                # re-registering. The node's re-register loop refreshes
                # last_seen every 60s. If it stops, we evict after the
                # stale threshold. No HTTP health check — the controller
                # may be in a container that can't reach the node's LAN IP.
                if (now - node.last_seen) > stale_threshold * 3:
                    log.info("Node offline (no re-registration): %s", nid)
                    await self._state.remove_node(nid)
            else:
                # mDNS nodes: check staleness from last announcement
                if (now - node.last_seen) > stale_threshold:
                    log.info("Node stale, marking offline: %s", nid)
                    await self._state.remove_node(nid)

        # Re-query mDNS nodes — re-resolve TXT records to pick up
        # fields that may have been missing on first discovery.
        if self._azc is None:
            return
        service_type = self._config.mdns_service_type
        for name, node in list(self._state.nodes.items()):
            if node.direct_registered:
                continue  # no mDNS to re-resolve
            try:
                await self._resolve_and_add(self._azc.zeroconf, service_type, name)
            except Exception as e:
                log.debug("Requery failed for %s: %s", name, e)

