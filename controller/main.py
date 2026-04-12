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
from mobile_camera import MobileCameraManager
from obs_studio import OBSStudioManager
from stream_router import StreamRouter
from guacamole import GuacamoleManager
from provisioning import ProvisioningManager
from auth import AuthConfig, setup_auth_password
from users import UserManager
from vaultwarden import VaultwardenManager, VaultwardenConfig
from email_security import EmailSecurityMonitor
from cloud_backup import CloudBackupManager
from itsm import ITSMManager
from license_manager import LicenseManager
from job_queue import JobQueue
from key_store import KeyStore
from mdm_bridge import MDMBridgeManager, MDMConfig
from network_scan import NetworkScanManager, NetworkScanConfig
from dlp import DLPManager, DLPConfig
from saas_management import SaaSManager
from threat_intelligence import ThreatIntelligenceEngine
from camera_recording import CameraRecordingManager
from wifi_ap import WiFiAPManager
from router_mode import RouterModeManager
from dns_filter import DNSFilterManager, DNS_FILTER_CONF_DIR
from local_proxy import LocalProxyManager
from file_sharing import FileSharingManager
from zfs_manager import ZFSManager
from failover import FailoverManager, FailoverMode
from ups_monitor import UPSMonitor
from ddns import DDNSManager
from speedtest_monitor import SpeedtestMonitor
from backup_status import BackupStatusTracker, BackupNudgeService
from game_streaming import SunshineManager
from auto_configure import AutoConfigureManager
from camera_connect import CameraConnectManager
from grid import GridService
from parental_controls import ParentalControlsManager
from compliance_reports import ComplianceReportEngine
from msp_dashboard import MSPDashboardManager
from msp_portal import MSPPortalManager, PortalConfig
from service_proxy import ServiceProxyManager
from idp import IdentityProvider, IdPConfig
from sharing import SharingManager
from external_publish import ExternalPublishManager
from wg_peering import WGPeeringManager
from node_reconciler import NodeReconciler
from alerts import AlertManager
from doorbell import DoorbellManager
from dns_verify import DNSVerifier
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
    mob_cam = MobileCameraManager(camera_mgr=camera_mgr, hls_dir=Path(__file__).parent / "static" / "cameras")
    obs_studio = OBSStudioManager()
    stream_router = StreamRouter(codec_manager=codec_mgr)
    guac_mgr = GuacamoleManager(state=state)
    provision_mgr = ProvisioningManager(state=state)
    notifier = NotificationManager()
    alert_mgr = AlertManager(state=state, kdeconnect=kdeconnect, notifier=notifier)
    await alert_mgr.start()
    doorbell_mgr = DoorbellManager(
        state=state,
        frigate_url=os.environ.get("OZMA_FRIGATE_URL", "http://localhost:5000"),
        alert_mgr=alert_mgr,
    )
    await doorbell_mgr.start()
    controls = ControlManager(state, scenarios, audio, motion, doorbell=doorbell_mgr, alerts=alert_mgr)

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
    recorder = SessionRecorder()
    net_health = NetworkHealthMonitor(state)
    metrics_collector = MetricsCollector(state)
    await metrics_collector.start()
    screen_mgr = ScreenManager(metrics=metrics_collector, state=state)
    await screen_mgr.start()
    screen_ws = ScreenWebSocketServer(screen_mgr)
    await screen_ws.start()
    await net_health.start()
    await state.measurement_engine.start()
    await state.binding_loop.start()

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

    # Pull latest mic curves (phone + USB) from Connect (non-blocking)
    async def _refresh_mic_curves():
        if connect and room_corr:
            curves = await connect.get_mic_curves()
            if curves:
                # curves is already the curves dict (may be nested phone/usb or flat)
                n = room_corr.update_mic_curves(curves)
                if n:
                    log.info("Updated %d mic compensation curve(s) from Connect", n)

    asyncio.create_task(_refresh_mic_curves(), name="mic_curves_refresh")

    # Daily mic curve refresh loop
    async def _mic_curves_refresh_loop():
        while True:
            await asyncio.sleep(86400)  # 24 hours
            await _refresh_mic_curves()

    asyncio.create_task(_mic_curves_refresh_loop(), name="mic_curves_refresh_loop")

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

    # Authentication setup — include mesh IPv6 ULA in WireGuard bypass.
    # Read the ULA prefix from mesh config directly (the canonical
    # MeshNetworkManager instance lives in api.py — don't create a second).
    _mesh_bypass = ["10.200.0.0/16"]
    try:
        from mesh_network import CONFIG_PATH as _mesh_cfg_path
        if _mesh_cfg_path.exists():
            import json as _json
            _mesh_data = _json.loads(_mesh_cfg_path.read_text())
            if ula := _mesh_data.get("ula_prefix"):
                _mesh_bypass.append(f"{ula}::/48")
    except Exception:
        pass
    auth_cfg = AuthConfig(
        enabled=config.auth_enabled,
        wireguard_bypass_subnets=_mesh_bypass,
    )
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

    # Vaultwarden — self-hosted password manager
    vw_cfg = VaultwardenConfig(
        enabled=config.vaultwarden_enabled,
        data_dir=config.vaultwarden_data_dir,
        port=config.vaultwarden_port,
        admin_token=config.vaultwarden_admin_token,
    )
    vault_mgr = VaultwardenManager(vw_cfg, controller_dir=Path(__file__).parent)
    await vault_mgr.start()
    state.vaultwarden_manager = vault_mgr

    # SSH bastion — terminal access to mesh nodes via SSH
    from ssh_bastion import SSHBastionServer, BastionConfig
    bastion_cfg = BastionConfig(
        enabled=config.ssh_bastion_enabled,
        port=config.ssh_bastion_port,
    )
    ssh_bastion = SSHBastionServer(
        bastion_cfg, state=state, audit=None,
        auth_config=auth_cfg, user_manager=user_mgr,
    )
    await ssh_bastion.start()

    # Email security monitor
    async def _email_alert(domain: str, posture) -> None:
        high = [i for i in posture.issues if i.severity in ("critical", "high")]
        if high:
            await state.events.put({
                "type": "email_security.alert",
                "domain": domain,
                "grade": posture.grade,
                "score": posture.score,
                "issues": [i.to_dict() for i in high],
            })
            if notifier:
                await notifier.on_event("email_security.alert", {
                    "domain": domain, "grade": posture.grade,
                    "issues": [i.to_dict() for i in high],
                })

    email_sec = EmailSecurityMonitor(on_alert=_email_alert)
    await email_sec.start()

    # Cloud backup (M365 + Google Workspace)
    cloud_backup_dir = Path(__file__).parent / "cloud_backup"
    mesh_key_bytes: bytes | None = None
    if mesh_ca and mesh_ca.controller_keypair:
        try:
            mesh_key_bytes = bytes.fromhex(
                mesh_ca.controller_keypair.private_key_hex()
            )
        except Exception:
            pass
    cloud_backup = CloudBackupManager(cloud_backup_dir, mesh_key_bytes)
    await cloud_backup.start()

    # ITSM — ticket triage, on-call scheduling, agent escalation
    itsm_data_dir = Path(__file__).parent / "itsm_data"
    itsm_mgr = ITSMManager(itsm_data_dir, notifier=notifier, event_queue=state.events)
    await itsm_mgr.start()

    # License & SaaS management
    license_mgr = LicenseManager(
        on_alert=notifier.on_event if notifier else None,
    )
    await license_mgr.start()

    # MDM Bridge (Google Workspace / Intune / Jamf)
    mdm_data_dir = Path(__file__).parent / "mdm_data"
    mdm_mgr = MDMBridgeManager(mdm_data_dir, event_queue=state.events)
    await mdm_mgr.start()

    # Network scanning (nmap + nuclei + OpenVAS + Nessus)
    scan_data_dir = Path(__file__).parent / "scan_data"
    net_scan_mgr = NetworkScanManager(scan_data_dir, event_queue=state.events)
    net_scan_mgr.itsm = itsm_mgr
    await net_scan_mgr.start()

    # DLP — data loss prevention
    dlp_data_dir = Path(__file__).parent / "dlp_data"
    dlp_mgr = DLPManager(dlp_data_dir, event_queue=state.events)
    dlp_mgr.itsm = itsm_mgr
    await dlp_mgr.start()

    # SaaS management — discovery, governance, cost optimisation
    saas_data_dir = Path(__file__).parent / "saas_data"
    saas_mgr = SaaSManager(saas_data_dir, event_queue=state.events)
    await saas_mgr.start()

    # Threat intelligence — KEV, ACSC advisories, credential exposure, ATT&CK
    threat_data_dir = Path(__file__).parent / "threat_data"
    threat_intel = ThreatIntelligenceEngine(threat_data_dir, event_queue=state.events)
    threat_intel.itsm = itsm_mgr
    await threat_intel.start()

    # Compliance report engine — E8, ISO 27001, SOC 2, CIS
    compliance_data_dir = Path(__file__).parent / "compliance_data"
    compliance_engine = ComplianceReportEngine(compliance_data_dir, event_queue=state.events)
    compliance_engine.inject_managers(
        threat_intel=threat_intel,
        network_scan=net_scan_mgr,
        dlp=dlp_mgr,
        mdm=mdm_mgr,
        itsm=itsm_mgr,
        saas_mgr=saas_mgr,
        itam=license_mgr,
    )
    await compliance_engine.start()

    # MSP multi-tenant dashboard
    msp_data_dir = Path(__file__).parent / "msp_data"
    msp_mgr = MSPDashboardManager(data_dir=msp_data_dir, event_queue=state.events)
    await msp_mgr.start()
    msp_portal_mgr = MSPPortalManager(msp_mgr=msp_mgr, config=PortalConfig())

    # IoT VLAN management
    from iot_network import IoTNetworkManager
    iot_mgr = IoTNetworkManager()
    await iot_mgr.start()

    # Built-in Wi-Fi AP (IoT SSID + onboarding SSID via hostapd)
    wifi_ap_mgr = WiFiAPManager()
    await wifi_ap_mgr.start()

    # DNS/ad filter — blocklist-based NXDOMAIN filtering via dnsmasq conf-dir
    dns_filter_mgr = DNSFilterManager()
    await dns_filter_mgr.start()

    # Router mode (NAT + DHCP + DNS + IoT nftables + camera VLAN exemption)
    # Passes conf-dir so dnsmasq picks up the DNS filter blocklist automatically
    router_mgr = RouterModeManager(dns_filter_conf_dir=str(DNS_FILTER_CONF_DIR))
    await router_mgr.start()

    # Local reverse proxy — Caddy-based LAN HTTPS for home services
    local_proxy_mgr = LocalProxyManager()
    await local_proxy_mgr.start()

    # File sharing — Samba + NFS (ZFS-backed shares get shadow_copy2 / Previous Versions)
    file_sharing_mgr = FileSharingManager()
    await file_sharing_mgr.start()

    # ZFS pool/dataset/snapshot management + cloud backup via zfs send
    zfs_mgr = ZFSManager(event_queue=state.events)
    await zfs_mgr.start()

    # Business continuity failover — heartbeat to Connect, virtual controller
    # support, and state sync on recovery. Works in both local and virtual modes.
    failover_mgr = FailoverManager(
        connect=connect,
        state=state,
        scenarios=scenarios,
    )
    await failover_mgr.start()

    # UPS / power management (NUT)
    ups_monitor = UPSMonitor(event_queue=state.events)
    await ups_monitor.start()

    # Dynamic DNS
    ddns_mgr = DDNSManager()
    await ddns_mgr.start()

    # WAN speed monitoring
    speedtest_mgr = SpeedtestMonitor(event_queue=state.events)
    await speedtest_mgr.start()

    # DNS integrity verification — checks resolver health, DNSSEC, rebinding, captive portals
    dns_verifier = DNSVerifier()
    await dns_verifier.start()

    # Job queue — persistent async task queue for agent/node operations
    job_queue = JobQueue(state_ref=state)
    await job_queue.start()

    # Master key store — memory-only after unlock; wraps all ZK subkeys
    key_store = KeyStore(controller_id=config.controller_id if hasattr(config, "controller_id") else "")
    await key_store.start()

    # Fleet backup status tracker — aggregates per-node backup reports
    backup_state = Path(__file__).parent / "backup_fleet_status.json"
    backup_tracker = BackupStatusTracker(state_path=backup_state)

    # Backup default-on nudge — fires backup.not_configured events for unconfigured nodes
    backup_nudge = BackupNudgeService(state=state, tracker=backup_tracker,
                                      event_queue=state.events)
    await backup_nudge.start()

    # Game streaming (V1.2) — Sunshine/Moonlight manager
    sunshine_data = Path(__file__).parent / "sunshine_data"
    sunshine_mgr = SunshineManager(data_dir=sunshine_data, state=state)
    await sunshine_mgr.start()

    # Auto-configure (V1.7) — PoE subnet device discovery + camera auto-registration
    ac_data = Path(__file__).parent / "auto_configure_data"
    auto_configure_mgr = AutoConfigureManager(state=state, data_dir=ac_data)
    await auto_configure_mgr.start()

    # Grid federation (V1.4) — multi-Desk KVM federation, claims, feeds, failover
    grid_data = Path(__file__).parent / "grid_data"
    grid_svc = GridService(
        name=getattr(config, "grid_name", "Ozma Grid"),
        port=getattr(config, "grid_port", 7381),
        data_dir=grid_data,
    )
    await grid_svc.start()

    # Parental controls (V1.8) — child profiles, app whitelists, timers, schedules
    parental_data = Path(__file__).parent / "parental_data"
    parental_mgr = ParentalControlsManager(data_dir=parental_data)
    await parental_mgr.start()

    # Camera Connect (V1.7) — proxy camera nodes to Connect cloud for remote access
    cc_data = Path(__file__).parent / "camera_connect_data"
    cam_connect_mgr = CameraConnectManager(
        state=state,
        connect=connect if "connect" in dir() else None,
        data_dir=cc_data,
    )
    await cam_connect_mgr.start()

    # Camera recording — policies, triggers, ZK-encrypted storage backends
    cam_rec_data = Path(__file__).parent / "recording_data"
    cam_rec_mgr = CameraRecordingManager(
        data_dir=cam_rec_data,
        key_store=key_store,
        state_ref=state,
        event_queue=state.events,
        connect=connect if "connect" in dir() else None,
    )
    await cam_rec_mgr.start()

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
        ota_pub_path = Path(__file__).parent / "ota_signing.pub"
        ota_pubkey: bytes | None = None
        if ota_pub_path.exists():
            try:
                ota_pubkey = bytes.fromhex(ota_pub_path.read_text().strip())
            except ValueError:
                log.warning("Invalid OTA public key in %s — signature verification disabled",
                            ota_pub_path)
        else:
            log.warning("OTA signing public key not found (%s) — signature verification disabled",
                        ota_pub_path)
        update_mgr = UpdateManager(firmware_ca_pubkey=ota_pubkey)
        asyncio.create_task(update_mgr.check_loop(), name="update-checker")

    # Live transcription — optional; requires whisper.cpp on PATH
    transcription_mgr = None
    if config.transcription_enabled:
        from live_transcription import LiveTranscriptionManager
        transcription_mgr = LiveTranscriptionManager(connect=connect)

    # WireGuard inter-controller peering (needs ctrl_id derived from mesh_ca)
    _ctrl_id_for_wg = (
        mesh_ca.controller_keypair.fingerprint()
        if mesh_ca and mesh_ca.controller_keypair
        else "ozma-controller"
    )
    wg_mgr = WGPeeringManager(controller_id=_ctrl_id_for_wg, api_port=config.api_port)

    # Build the FastAPI app — all managers must be created before this point
    app = build_app(state, scenarios, streams, audio, controls, rgb_out, motion, bt, kdeconnect, wifi_audio, captures, paste_typer, kbd_mgr, macro_mgr, sched, notifier, recorder, net_health, ocr_triggers, auto_engine, metrics_collector, screen_mgr, codec_mgr=codec_mgr, camera_mgr=camera_mgr, obs_studio=obs_studio, stream_router=stream_router, guac_mgr=guac_mgr, provision_mgr=provision_mgr, connect=connect, mesh_ca=mesh_ca, sess_mgr=sess_mgr, room_correction=room_corr, testbench=testbench, agent_engine=agent_engine, test_runner=test_runner, auth_config=auth_cfg, user_manager=user_mgr, service_proxy=svc_proxy, idp=idp_instance, sharing=sharing_mgr, ext_publish=ext_pub, node_reconciler=reconciler, update_mgr=update_mgr, transcription_mgr=transcription_mgr, discovery=discovery, doorbell_mgr=doorbell_mgr, alert_mgr=alert_mgr, vaultwarden=vault_mgr, email_security=email_sec, cloud_backup=cloud_backup, iot=iot_mgr, wg=wg_mgr, itsm=itsm_mgr, license_mgr=license_mgr, mdm=mdm_mgr, job_queue=job_queue, net_scan=net_scan_mgr, key_store=key_store, dlp=dlp_mgr, saas_mgr=saas_mgr, threat_intel=threat_intel, compliance=compliance_engine, cam_rec=cam_rec_mgr, wifi_ap=wifi_ap_mgr, router=router_mgr, backup_tracker=backup_tracker, mobile_cam=mob_cam, sunshine=sunshine_mgr, msp_mgr=msp_mgr, msp_portal=msp_portal_mgr, auto_configure=auto_configure_mgr, cam_connect=cam_connect_mgr, grid=grid_svc, parental=parental_mgr, backup_nudge=backup_nudge, dns_filter=dns_filter_mgr, local_proxy=local_proxy_mgr, file_sharing=file_sharing_mgr, zfs=zfs_mgr, failover=failover_mgr, ups_monitor=ups_monitor, ddns=ddns_mgr, speedtest=speedtest_mgr, dns_verifier=dns_verifier)

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
    ctrl_id = (
        mesh_ca.controller_keypair.fingerprint()
        if mesh_ca and mesh_ca.controller_keypair
        else "ozma-controller"
    )
    await discovery.announce_controller(ctrl_id, api_port=config.api_port)

    # WireGuard inter-controller peering start
    try:
        await wg_mgr.start()
    except Exception as e:
        log.warning("WireGuard peering start failed (ip/wg not available?): %s", e)

    # Auto-link LAN peer controllers discovered via mDNS
    async def _on_peer_found(info: dict) -> None:
        if not sharing:
            return
        existing = sharing.get_peer(info["id"])
        if existing:
            # Update address in case it changed; mark online
            was_online = existing.online
            updated = sharing.mark_peer_online(info["id"], info["host"], info["api_port"])
            if updated and not was_online:
                await state.events.put({"type": "peer.online", "controller_id": info["id"]})
        else:
            peer = sharing.add_peer(
                controller_id=info["id"],
                owner_user_id="",
                name=info["id"],
                host=info["host"],
                port=info["api_port"],
                transport="lan",
            )
            peer.auto_discovered = True
            await state.events.put({"type": "peer.discovered", "peer": peer.to_dict()})
            log.info("Auto-linked LAN peer: %s @ %s:%d", info["id"], info["host"], info["api_port"])
            # Initiate WireGuard peering with the newly discovered controller
            asyncio.create_task(
                wg_mgr.peer_with(info["host"], info["api_port"]),
                name=f"wg-peer-{info['id'][:8]}",
            )

    async def _on_peer_lost(ctrl_id: str) -> None:
        if not sharing:
            return
        peer = sharing.mark_peer_offline(ctrl_id)
        if peer:
            await state.events.put({"type": "peer.offline", "controller_id": ctrl_id})

    await discovery.start_peer_browser(_on_peer_found, _on_peer_lost)

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
    # Stop GraphQL subscription event router
    from controller.graphql.subscriptions import stop_event_router
    stop_event_router()
    await hid.stop()
    await controls.stop()
    await kdeconnect.stop()
    await bt.stop()
    await wifi_audio.stop()
    await motion.stop()
    await rgb_out.stop()
    await audio.stop()
    await discovery.withdraw_controller()
    await discovery.stop()
    await net_health.stop()
    await state.measurement_engine.stop()
    await state.binding_loop.stop()
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
    await vault_mgr.stop()
    await email_sec.stop()
    await cloud_backup.stop()
    await license_mgr.stop()
    await job_queue.stop()
    await key_store.stop()
    await mdm_mgr.stop()
    await net_scan_mgr.stop()
    await dlp_mgr.stop()
    await saas_mgr.stop()
    await threat_intel.stop()
    await compliance_engine.stop()
    await msp_mgr.stop()
    await cam_rec_mgr.stop()
    await wifi_ap_mgr.stop()
    await router_mgr.stop()
    await sunshine_mgr.stop()
    await auto_configure_mgr.stop()
    await cam_connect_mgr.stop()
    await grid_svc.stop()
    await parental_mgr.stop()
    await backup_nudge.stop()
    await dns_filter_mgr.stop()
    await local_proxy_mgr.stop()
    await file_sharing_mgr.stop()
    await zfs_mgr.stop()
    await failover_mgr.stop()
    await ups_monitor.stop()
    await ddns_mgr.stop()
    await speedtest_mgr.stop()
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
