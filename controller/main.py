#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma Controller daemon.

Starts:
  - mDNS listener (_ozma._udp.local) — discovers compute nodes
  - HID capture + UDP forwarder — reads evdev, sends to active node
  - REST + WebSocket API on port 7380

Usage:
  python main.py [--debug] [--kbd /dev/input/eventN] [--mouse /dev/input/eventN]

Environment variables (override defaults):
  OZMA_API_HOST       bind address for REST/WS server (default 0.0.0.0)
  OZMA_API_PORT       port for REST/WS server (default 7380)
  OZMA_NODE_PORT      UDP port on nodes (default 7331)
  OZMA_KBD_DEVICE     path to keyboard evdev device
  OZMA_MOUSE_DEVICE   path to mouse evdev device
  OZMA_DEBUG          set to 1 to enable debug logging
"""

import argparse
import asyncio
import logging
import os
import signal
import sys
from pathlib import Path

import uvicorn

from config import Config
from state import AppState
from discovery import DiscoveryService
from hid import HIDForwarder
from scenarios import ScenarioManager
from rgb import RGBEngine
from stream import StreamManager
from audio import AudioRouter
from controls import ControlManager, ControlSurface, Control, ControlBinding
from midi import MidiSurface
from gamepad import GamepadSurface, find_gamepad_devices
from rgb_outputs import RGBOutputManager
from motion import MotionManager, MotionDevice, MotionAxis, MotionPreset
from bluetooth import BluetoothManager
from kdeconnect import KDEConnectBridge
from display_capture import DisplayCaptureManager
from paste_typing import PasteTyper
from device_metrics import MetricsCollector
from screen_manager import ScreenManager
from screen_server import ScreenWebSocketServer
from keyboard_manager import KeyboardManager
from macros import MacroManager
from scheduler import Scheduler
from notifications import NotificationManager
from session_recording import SessionRecorder
from network_health import NetworkHealthMonitor
from ocr_triggers import OCRTriggerManager
from automation import AutomationEngine
from testbench import TestBench
from agent_engine import AgentEngine
from test_runner import TestRunner
from mcp_server import start_mcp_server
from rtp_receiver import RTPReceiverManager
from wifi_audio_receiver import WiFiAudioManager
from streamdeck_surface import StreamDeckSurface, discover_streamdecks
from osc_surface import OSCSurface
from evdev_surface import EvdevSurface
from codec_manager import CodecManager
from connect import OzmaConnect
from room_correction import RoomCorrectionManager
from pairing import MeshCA
from session import SessionManager
from camera_manager import CameraManager
from obs_studio import OBSStudioManager
from stream_router import StreamRouter
from guacamole import GuacamoleManager
from provisioning import ProvisioningManager
from auth import AuthConfig, setup_auth_password
from users import UserManager
from service_proxy import ServiceProxyManager
from idp import IdentityProvider, IdPConfig
from sharing import SharingManager
from external_publish import ExternalPublishManager
from node_reconciler import NodeReconciler
from api import build_app


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Ozma Controller daemon")
    p.add_argument("--debug", action="store_true", help="Enable debug logging")
    p.add_argument("--kbd", metavar="DEVICE", help="Keyboard evdev path (e.g. /dev/input/event0)")
    p.add_argument("--mouse", metavar="DEVICE", help="Mouse evdev path (e.g. /dev/input/event1)")
    p.add_argument("--host", default=None, help="API bind address")
    p.add_argument("--port", type=int, default=None, help="API port")
    p.add_argument("--virtual-only", action="store_true",
                   help="Only capture ozma-virtual-* input devices (test harness mode)")
    return p.parse_args()


def setup_logging(debug: bool) -> None:
    level = logging.DEBUG if debug else logging.INFO
    logging.basicConfig(
        level=level,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )


log = logging.getLogger("ozma")


async def run(config: Config) -> None:
    state = AppState()

    scenarios_path = Path(__file__).parent / "scenarios.json"
    _yaml_path = Path(__file__).parent / "scenarios.yaml"
    if not scenarios_path.exists() and _yaml_path.exists():
        scenarios_path = _yaml_path
    rgb_engine = RGBEngine()
    audio = AudioRouter(
        state,
        output_sink=config.audio_output_sink or None,
        mic_source=config.audio_mic_source or None,
        enabled=config.audio_enabled,
        wireplumber_mode=config.audio_wireplumber,
    )
    rgb_out = RGBOutputManager()
    rgb_out.set_state(state)
    motion = MotionManager()
    bt = BluetoothManager()
    kdeconnect = KDEConnectBridge()

    async def _kdeconnect_event(event_type: str, data: dict) -> None:
        """Forward KDE Connect events to WebSocket clients and RGB compositor."""
        await state.events.put({"type": event_type, **data})
        # Phone notification → RGB note
        if event_type == "kdeconnect.notification":
            rgb_out.compositor.add_note(
                f"phone-{data.get('app', 'notif')}", color=(100, 180, 255), ttl=3.0, effect="flash"
            )
        # Incoming call → system alert
        elif event_type == "kdeconnect.telephony" and data.get("event") == "ringing":
            rgb_out.compositor.add_note("phone-ringing", color=(50, 255, 50), ttl=10.0, effect="pulse")

    kdeconnect.on_event = _kdeconnect_event
    # Security: mesh CA + session manager
    mesh_ca = MeshCA()
    mesh_ca.initialise()
    sess_mgr = SessionManager(mesh_ca)
    log.info("Mesh CA: %s (%d paired nodes)",
             mesh_ca.status()["ca_fingerprint"], mesh_ca.status()["paired_nodes"])

    # Ozma Connect (SaaS client)
    connect = OzmaConnect()
    await connect.start()

    codec_mgr = CodecManager()
    log.info("Codecs available: %s", {k: len(v) for k, v in codec_mgr.list_available().items() if v})

    scenarios = ScenarioManager(scenarios_path, state, rgb_engine=rgb_engine,
                                audio_router=audio, rgb_outputs=rgb_out,
                                motion_manager=motion, bluetooth=bt)
    streams = StreamManager(state, codec_manager=codec_mgr)
    captures = DisplayCaptureManager(codec_manager=codec_mgr)
    camera_mgr = CameraManager(codec_manager=codec_mgr)
    obs_studio = OBSStudioManager()
    stream_router = StreamRouter(codec_manager=codec_mgr)
    guac_mgr = GuacamoleManager(state=state)
    provision_mgr = ProvisioningManager(state=state)
    controls = ControlManager(state, scenarios, audio, motion)

    # Built-in hotkey surface: ScrollLock = next scenario, Pause = prev
    hotkey_surface = ControlSurface("hotkeys")
    hotkey_surface.controls["next_scenario"] = Control(
        name="next_scenario", surface_id="hotkeys",
        binding=ControlBinding(action="scenario.next", value=1),
    )
    hotkey_surface.controls["prev_scenario"] = Control(
        name="prev_scenario", surface_id="hotkeys",
        binding=ControlBinding(action="scenario.next", value=-1),
    )
    controls.register_surface(hotkey_surface)

    # Auto-detect gamepads
    for gpad_dev in find_gamepad_devices():
        gpad = GamepadSurface(gpad_dev)
        gpad.set_on_changed(controls.on_control_changed)
        controls.register_surface(gpad)

    # Auto-detect Stream Decks
    for deck in discover_streamdecks():
        sd = StreamDeckSurface(deck)
        sd.set_on_changed(controls.on_control_changed)
        controls.register_surface(sd)

    # Load additional surfaces from controls.yaml if configured
    if config.controls_config:
        import yaml
        try:
            controls_yaml = yaml.safe_load(Path(config.controls_config).read_text())
            for sid, scfg in (controls_yaml or {}).get("surfaces", {}).items():
                stype = scfg.get("type", "")
                if stype == "midi":
                    surface = MidiSurface(sid, scfg)
                    surface.set_on_changed(controls.on_control_changed)
                    controls.register_surface(surface)
                elif stype == "osc":
                    surface = OSCSurface(
                        surface_id=sid,
                        listen_host=scfg.get("listen_host", "0.0.0.0"),
                        listen_port=scfg.get("listen_port", 9000),
                        feedback_host=scfg.get("feedback_host"),
                        feedback_port=scfg.get("feedback_port", 9001),
                    )
                    surface.set_on_changed(controls.on_control_changed)
                    controls.register_surface(surface)
                elif stype == "evdev":
                    surface = EvdevSurface(sid, scfg)
                    surface.set_on_changed(controls.on_control_changed)
                    controls.register_surface(surface)

            # Load motion devices
            for mid, mcfg in (controls_yaml or {}).get("motion", {}).items():
                axes = {
                    name: MotionAxis(name=name, mode=acfg.get("mode", "velocity"))
                    for name, acfg in mcfg.get("axes", {}).items()
                }
                presets = {
                    pname: MotionPreset(name=pname, axes=pcfg)
                    for pname, pcfg in mcfg.get("presets", {}).items()
                }
                device = MotionDevice(
                    id=mid, name=mcfg.get("name", mid),
                    device_type=mcfg.get("type", "serial"),
                    axes=axes, presets=presets, props=mcfg,
                )
                motion.add_device(device)
        except FileNotFoundError:
            log.warning("Controls config not found: %s", config.controls_config)
        except Exception as e:
            log.warning("Failed to load controls config: %s", e)

    discovery = DiscoveryService(config, state)
    hid = HIDForwarder(config, state, streams=streams, control_manager=controls,
                       session_manager=sess_mgr)

    # Register default hotkeys (evdev keycodes)
    from evdev import ecodes
    hid.register_hotkey(ecodes.KEY_SCROLLLOCK, "next_scenario", "hotkeys")
    hid.register_hotkey(ecodes.KEY_PAUSE, "prev_scenario", "hotkeys")

    # Graceful shutdown on SIGINT/SIGTERM
    loop = asyncio.get_running_loop()
    stop_event = asyncio.Event()

    def _on_signal() -> None:
        stop_event.set()

    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, _on_signal)

    await scenarios.start()
    await streams.start()
    await captures.start()
    rtp_rx = RTPReceiverManager(state)
    await rtp_rx.start()
    paste_typer = PasteTyper(state)
    kbd_mgr = KeyboardManager()
    await kbd_mgr.start()
    macro_mgr = MacroManager(state, paste_typer)
    sched = Scheduler(scenarios)
    await sched.start()
    notifier = NotificationManager()
    recorder = SessionRecorder()
    net_health = NetworkHealthMonitor(state)
    metrics_collector = MetricsCollector(state)
    await metrics_collector.start()
    screen_mgr = ScreenManager(metrics=metrics_collector, state=state)
    await screen_mgr.start()
    screen_ws = ScreenWebSocketServer(screen_mgr)
    await screen_ws.start()
    await net_health.start()

    # OCR triggers — watch screens for errors
    from text_capture import TextCapture
    text_ocr = TextCapture()
    ocr_triggers = OCRTriggerManager()

    async def _ocr_trigger(pattern_id, severity, source_id, data):
        await state.events.put({"type": "ocr.trigger", **data})
        rgb_out.compositor.set_system_alert(f"ocr-{pattern_id}", color=(255, 0, 0), effect="strobe")
        if notifier:
            await notifier.on_event("ocr.trigger", data)

    ocr_triggers.on_trigger = _ocr_trigger
    await ocr_triggers.start(text_ocr, captures)

    # Automation engine
    auto_engine = AutomationEngine(state, text_ocr, captures)
    wifi_audio = WiFiAudioManager()
    room_corr = RoomCorrectionManager()

    # TestBench
    testbench = TestBench(state, auto_engine, metrics_collector, captures, recorder)

    # Vision providers (OmniParser, YOLO, Ollama, Connect)
    from vision_providers import VisionProviderManager
    connect_url = getattr(connect, '_url', '') if connect else ''
    connect_key = getattr(connect, '_api_key', '') if connect else ''
    vision_mgr = VisionProviderManager.create_default(
        connect_url=connect_url, connect_key=connect_key,
    )
    available = [p for p in vision_mgr.list_providers() if p["available"]]
    if available:
        log.info("Vision providers: %s", ", ".join(p["name"] for p in available))

    # AI Agent Engine
    from screen_reader import ScreenReader
    agent_screen_reader = ScreenReader(vision_manager=vision_mgr)
    agent_engine = AgentEngine(state, agent_screen_reader, text_ocr,
                               notifier=notifier, event_queue=state.events)

    # Visual regression test runner
    test_runner = TestRunner(agent_engine, notifier)

    # Authentication setup
    auth_cfg = AuthConfig(enabled=config.auth_enabled)
    if config.auth_password_hash:
        auth_cfg.password_hash = config.auth_password_hash
    elif config.auth_enabled:
        env_pw = os.environ.get("OZMA_AUTH_PASSWORD")
        pw_hash, pw_plain = setup_auth_password(env_pw)
        auth_cfg.password_hash = pw_hash
        if not env_pw:
            log.warning("=" * 60)
            log.warning("  GENERATED ADMIN PASSWORD: %s", pw_plain)
            log.warning("  Set OZMA_AUTH_PASSWORD env var to use your own.")
            log.warning("=" * 60)

    # User management
    users_path = Path(__file__).parent / "users.json"
    user_mgr = UserManager(users_path)

    # Migrate single-admin password to a User on first run with auth enabled
    if config.auth_enabled and not user_mgr.has_users() and auth_cfg.password_hash:
        user_mgr.create_user_with_hash(
            username="admin", display_name="Admin",
            password_hash=auth_cfg.password_hash, role="owner",
        )
        log.info("Migrated admin password to user account")

    state.user_manager = user_mgr

    # Service proxy
    services_path = Path(__file__).parent / "services.json"
    svc_proxy = ServiceProxyManager(services_path)
    await svc_proxy.start()

    # Identity provider (OIDC)
    idp_instance: IdentityProvider | None = None
    if config.idp_enabled and config.auth_enabled:
        idp_cfg = IdPConfig(enabled=True)
        # Load IdP config from idp_config.json if it exists
        idp_config_path = Path(__file__).parent / "idp_config.json"
        if idp_config_path.exists():
            try:
                import json as _json
                idp_cfg = IdPConfig.from_dict(_json.loads(idp_config_path.read_text()))
                idp_cfg.enabled = True
            except Exception as e:
                log.warning("Failed to load IdP config: %s", e)
        idp_instance = IdentityProvider(
            config=idp_cfg,
            user_manager=user_mgr,
            signing_key=mesh_ca.controller_keypair if mesh_ca else None,
        )
        log.info("Identity Provider enabled (social providers: %d)",
                 len(idp_cfg.social_providers))

    # Sharing (cross-user resource grants + peer controllers)
    shares_path = Path(__file__).parent / "shares.json"
    sharing_mgr = SharingManager(shares_path)
    await sharing_mgr.start()

    # External publishing
    publish_path = Path(__file__).parent / "publish.json"
    ext_pub = ExternalPublishManager(publish_path)

    # Node reconciler — always active; merges hw + sw nodes for the same machine
    reconciler = NodeReconciler(state)
    await reconciler.start()

    # Hardware front panel — optional; silently no-ops without I2C hardware
    front_panel = None
    if config.front_panel_enabled:
        from front_panel import FrontPanel
        front_panel = FrontPanel(
            state=state,
            scenarios=scenarios,
            controls=controls,
            audio=audio,
        )
        await front_panel.start()

    # A/B update manager — only meaningful on bare-metal appliance builds
    update_mgr = None
    if config.update_manager_enabled:
        from update_manager import UpdateManager
        ca_pubkey = mesh_ca.controller_keypair.public_key if mesh_ca and mesh_ca.controller_keypair else None
        update_mgr = UpdateManager(firmware_ca_pubkey=ca_pubkey)
        asyncio.create_task(update_mgr.check_loop(), name="update-checker")

    # Live transcription — optional; requires whisper.cpp on PATH
    transcription_mgr = None
    if config.transcription_enabled:
        from live_transcription import LiveTranscriptionManager
        transcription_mgr = LiveTranscriptionManager(connect=connect)

    # Build the FastAPI app — all managers must be created before this point
    app = build_app(state, scenarios, streams, audio, controls, rgb_out, motion, bt, kdeconnect, wifi_audio, captures, paste_typer, kbd_mgr, macro_mgr, sched, notifier, recorder, net_health, ocr_triggers, auto_engine, metrics_collector, screen_mgr, codec_mgr=codec_mgr, camera_mgr=camera_mgr, obs_studio=obs_studio, stream_router=stream_router, guac_mgr=guac_mgr, provision_mgr=provision_mgr, connect=connect, mesh_ca=mesh_ca, sess_mgr=sess_mgr, room_correction=room_corr, testbench=testbench, agent_engine=agent_engine, test_runner=test_runner, auth_config=auth_cfg, user_manager=user_mgr, service_proxy=svc_proxy, idp=idp_instance, sharing=sharing_mgr, ext_publish=ext_pub, node_reconciler=reconciler, update_mgr=update_mgr, transcription_mgr=transcription_mgr)

    uv_config = uvicorn.Config(
        app,
        host=config.api_host,
        port=config.api_port,
        log_level="debug" if config.debug else "info",
        access_log=config.debug,
    )
    server = uvicorn.Server(uv_config)

    await camera_mgr.start()
    await obs_studio.start()
    await stream_router.start()
    await guac_mgr.start()
    provision_mgr._automation = auto_engine
    provision_mgr._screen_mgr = screen_mgr
    provision_mgr._guacamole = guac_mgr
    provision_mgr._notifier = notifier
    await provision_mgr.start()

    # MCP server (SSE transport for remote AI agents)
    await start_mcp_server(agent_engine, state, scenarios, test_runner, port=7381)
    obs_studio.register_ozma_sources(captures=captures, cameras=camera_mgr)

    await audio.start()
    await rgb_out.start()
    await motion.start()
    await wifi_audio.start()
    await bt.start()
    await kdeconnect.start()
    await controls.start()
    await discovery.start()
    await hid.start()

    # Monitor for virtual capture devices from soft nodes
    async def _virtual_capture_monitor():
        """Watch for nodes with capture_device and register them."""
        registered: set[str] = set()
        while True:
            await asyncio.sleep(5)
            for node_id, node in list(state.nodes.items()):
                if node.capture_device and node_id not in registered:
                    source_id = f"virtual-{node_id.split('.')[0]}"
                    await captures.register_virtual_capture(
                        source_id, node.capture_device,
                        name=f"Virtual: {node_id.split('.')[0]}",
                    )
                    registered.add(node_id)

    asyncio.create_task(_virtual_capture_monitor(), name="virtual-capture-monitor")

    # Auto-establish encrypted sessions with paired nodes
    async def _session_monitor():
        """Establish sessions with paired nodes that come online."""
        established: set[str] = set()
        while True:
            await asyncio.sleep(3)
            for node_id in list(state.nodes.keys()):
                if node_id in established:
                    continue
                if sess_mgr.has_session(node_id):
                    established.add(node_id)
                    continue
                if mesh_ca.is_node_trusted(node_id):
                    # Paired node without a session — initiate
                    init_bytes = sess_mgr.create_session_init(node_id)
                    if init_bytes:
                        # Send session init to the node's API endpoint
                        node = state.nodes.get(node_id)
                        if node and node.api_port:
                            try:
                                import urllib.request
                                loop = asyncio.get_running_loop()
                                url = f"http://{node.host}:{node.api_port}/session/init"
                                def _post():
                                    req = urllib.request.Request(
                                        url, data=init_bytes,
                                        headers={"Content-Type": "application/octet-stream"},
                                        method="POST",
                                    )
                                    with urllib.request.urlopen(req, timeout=5) as r:
                                        return r.read()
                                accept_bytes = await loop.run_in_executor(None, _post)
                                session = sess_mgr.complete_session(node_id, accept_bytes)
                                if session:
                                    established.add(node_id)
                                    log.info("Encrypted session established: %s", node_id)
                            except Exception as e:
                                log.debug("Session init to %s failed: %s", node_id, e)

    asyncio.create_task(_session_monitor(), name="session-monitor")

    # Run uvicorn until stop_event fires
    server_task = asyncio.create_task(server.serve(), name="uvicorn")

    await stop_event.wait()

    logging.getLogger("ozma").info("Shutting down...")
    server.should_exit = True
    await server_task
    await hid.stop()
    await controls.stop()
    await kdeconnect.stop()
    await bt.stop()
    await wifi_audio.stop()
    await motion.stop()
    await rgb_out.stop()
    await audio.stop()
    await discovery.stop()
    await net_health.stop()
    await sched.stop()
    await kbd_mgr.stop()
    await sharing_mgr.stop()
    await svc_proxy.stop()
    await connect.stop()
    await provision_mgr.stop()
    await guac_mgr.stop()
    await stream_router.stop()
    await obs_studio.stop()
    await camera_mgr.stop()
    await rtp_rx.stop()
    await captures.stop()
    await streams.stop()
    await scenarios.stop()
    await reconciler.stop()
    if front_panel:
        await front_panel.stop()


def main() -> None:
    args = parse_args()

    config = Config.from_env()
    if args.debug:
        config.debug = True
    if args.kbd:
        config.keyboard_device = args.kbd
    if args.mouse:
        config.mouse_device = args.mouse
    if args.host:
        config.api_host = args.host
    if args.port:
        config.api_port = args.port
    if args.virtual_only:
        config.virtual_only = True

    setup_logging(config.debug)
    log.info("Ozma Controller starting (API on %s:%d)", config.api_host, config.api_port)

    asyncio.run(run(config))


if __name__ == "__main__":
    main()
