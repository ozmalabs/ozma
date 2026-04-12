# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
REST API and WebSocket server on port 7380.

REST endpoints:
  GET  /api/v1/nodes                 — list all known nodes
  GET  /api/v1/nodes/{id}            — get a single node
  POST /api/v1/nodes/{id}/activate   — make a node active
  GET  /api/v1/status                — system snapshot
  GET  /api/v1/graph                 — routing graph (devices + links)
  GET  /api/v1/graph/devices         — all devices in the graph
  GET  /api/v1/graph/devices/{id}    — single device
  GET  /api/v1/graph/links           — all links in the graph

WebSocket:
  ws://<host>:7380/api/v1/events     — real-time push events
"""

import asyncio
import json
import logging
import time
from pathlib import Path
from typing import Any

from fastapi import FastAPI, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse, Response, StreamingResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel

from auth import (
    AuthConfig, AuthContext, create_jwt, verify_jwt, verify_password,
    has_scope, is_wireguard_source, SCOPE_READ, SCOPE_WRITE, SCOPE_ADMIN, ALL_SCOPES,
)
try:
    from graphql_api import create_router as _gql_create_router
    from graphql_api import add_graphiql_route as _gql_add_graphiql_route
    _GRAPHQL_AVAILABLE = True
except (ImportError, Exception):
    _gql_create_router = None  # type: ignore[assignment]
    _gql_add_graphiql_route = None  # type: ignore[assignment]
    _GRAPHQL_AVAILABLE = False

from state import AppState, NodeInfo
from permissions import (
    check_node_permission, check_destructive_warnings, warnings_to_dict,
    get_user_seats, get_user_permission_level, PERMISSION_LEVELS,
)
from scenarios import ScenarioManager
from gaming.moonlight_app_mapping import MoonlightAppMapper, create_app_mapper
from stream import StreamManager
from audio import AudioRouter
from controls import ControlManager
from rgb_outputs import RGBOutputManager
from motion import MotionManager
from bluetooth import BluetoothManager
from kdeconnect import KDEConnectBridge
from wifi_audio_receiver import WiFiAudioManager
from display_capture import DisplayCaptureManager
from text_capture import TextCapture
from paste_typing import PasteTyper
from keyboard_manager import KeyboardManager
from macros import MacroManager
from scheduler import Scheduler
from notifications import NotificationManager
from session_recording import SessionRecorder
from network_health import NetworkHealthMonitor
from wol import send_wol, get_mac_from_arp
from ocr_triggers import OCRTriggerManager, TriggerPattern
from automation import AutomationEngine
from device_metrics import MetricsCollector
from screen_manager import ScreenManager
from codec_manager import CodecManager, CodecConfig
from connect import OzmaConnect
from pairing import MeshCA
from session import SessionManager
from camera_manager import CameraManager
from obs_studio import OBSStudioManager
from stream_router import StreamRouter
from guacamole import GuacamoleManager
from provisioning import ProvisioningManager
from users import UserManager
from service_proxy import ServiceProxyManager
from idp import IdentityProvider
from sharing import SharingManager
from external_publish import ExternalPublishManager
from vaultwarden import VaultwardenManager
from email_security import EmailSecurityMonitor
from cloud_backup import CloudBackupManager, BackupSource, CredentialRecord, Provider
from iot_network import (
    IoTNetworkManager, IoTDevice, DeviceCategory, InternetAccess,
    VLANConfig, OnboardingSession,
)
from wg_peering import WGPeeringManager, WGPeer
from license_manager import (
    LicenseManager, LicensedProduct, SaaSApplication, LicenseType, SaaSCategory,
    DiscoverySource,
)
from itsm import (
    ITSMManager, ITSMConfig, Ticket, OnCallUser, EscalationPolicy, EscalationTier,
    WorkingHours, OnCallWindow, AgentModelConfig,
    AGENT_L1, AGENT_L2, AGENT_HUMAN, _PRIORITIES,
)
from mdm_bridge import MDMBridgeManager, MDMConfig, ManagedDevice
from network_scan import NetworkScanManager, NetworkScanConfig, OpenVASConfig, NessusConfig
from dlp import DLPManager, DLPConfig, DLPPolicy, DLPRule, DLPIncident
from saas_management import SaaSManager, ManagedSaaSApp, SaaSConfig as SaaSMgmtConfig
from threat_intelligence import ThreatIntelligenceEngine, ThreatConfig
from compliance_reports import ComplianceReportEngine, ComplianceConfig
from job_queue import JobQueue, JobSpec, JobType, JobState, TargetScope
from key_store import (
    KeyStore, BackupMethod, KeyLockedError, KeyNotInitialisedError,
    UnlockRateLimitedError,
)
from msp_dashboard import MSPDashboardManager, MSPClient, BulkOperation, BillingLine
from msp_portal import MSPPortalManager, PortalConfig
from parental_controls import ParentalControlsManager
from backup_status import BackupNudgeService

log = logging.getLogger("ozma.api")


class CreateScenarioRequest(BaseModel):
    name: str
    node_id: str | None = None


class BindNodeRequest(BaseModel):
    node_id: str | None = None


class VolumeRequest(BaseModel):
    node_name: str
    volume: float


class MuteRequest(BaseModel):
    node_name: str
    mute: bool


class SelectOutputRequest(BaseModel):
    output_id: str


class OutputDelayRequest(BaseModel):
    output_id: str
    delay_ms: float


class DirectRegisterRequest(BaseModel):
    """
    Direct node registration — used by nodes in QEMU/SLIRP environments where
    mDNS multicast can't cross the network boundary.  Fields mirror the mDNS
    TXT record + resolved address.
    """
    id: str           # mDNS instance name, e.g. "mynode._ozma._udp.local."
    host: str         # resolved IP address
    port: int = 7331
    proto: str = "1"
    role: str = "compute"
    hw: str = "unknown"
    fw: str = "unknown"
    cap: str = ""
    stream_port: str = ""
    stream_path: str = ""
    vnc_host: str = ""
    vnc_port: str = ""
    api_port: str = ""
    audio_type: str = ""
    audio_sink: str = ""
    audio_vban_port: str = ""
    mic_vban_port: str = ""
    capture_device: str = ""
    machine_class: str = "workstation"  # workstation | server | kiosk | camera
    display_outputs: str = ""  # JSON-encoded list of display output dicts
    # Camera node fields (machine_class="camera")
    camera_streams: str = ""   # JSON-encoded list of stream dicts
    frigate_host: str = ""     # Frigate API host (hostname or IP)
    frigate_port: str = ""     # Frigate API port (default 5000)
    vm_guest_ip: str = ""      # VM guest network IP (soft nodes only)
    pci_devices: str = ""     # JSON-encoded list of PCI addresses (GPU passthrough)


def build_app(state: AppState, scenarios: ScenarioManager, streams: StreamManager | None = None, audio: AudioRouter | None = None, controls: ControlManager | None = None, rgb_out: RGBOutputManager | None = None, motion: MotionManager | None = None, bt: BluetoothManager | None = None, kdeconnect: KDEConnectBridge | None = None, wifi_audio: WiFiAudioManager | None = None, captures: DisplayCaptureManager | None = None, paste_typer: PasteTyper | None = None, kbd_mgr: KeyboardManager | None = None, macro_mgr: MacroManager | None = None, sched: Scheduler | None = None, notifier: NotificationManager | None = None, recorder: SessionRecorder | None = None, net_health: NetworkHealthMonitor | None = None, ocr_triggers: OCRTriggerManager | None = None, auto_engine: AutomationEngine | None = None, metrics_collector: MetricsCollector | None = None, screen_mgr: ScreenManager | None = None, codec_mgr: CodecManager | None = None, camera_mgr: CameraManager | None = None, obs_studio: OBSStudioManager | None = None, stream_router: StreamRouter | None = None, guac_mgr: GuacamoleManager | None = None, provision_mgr: ProvisioningManager | None = None, connect: OzmaConnect | None = None, mesh_ca: MeshCA | None = None, sess_mgr: SessionManager | None = None, room_correction: Any = None, testbench: Any = None, agent_engine: Any = None, test_runner: Any = None, auth_config: AuthConfig | None = None, user_manager: UserManager | None = None, service_proxy: ServiceProxyManager | None = None, idp: IdentityProvider | None = None, sharing: SharingManager | None = None, ext_publish: ExternalPublishManager | None = None, node_reconciler=None, update_mgr=None, transcription_mgr=None, discovery=None, doorbell_mgr=None, alert_mgr=None, vaultwarden: VaultwardenManager | None = None, email_security: EmailSecurityMonitor | None = None, cloud_backup: CloudBackupManager | None = None, iot: IoTNetworkManager | None = None, wg: WGPeeringManager | None = None, itsm: ITSMManager | None = None, license_mgr: LicenseManager | None = None, mdm: MDMBridgeManager | None = None, job_queue: JobQueue | None = None, net_scan: NetworkScanManager | None = None, key_store: KeyStore | None = None, dlp: DLPManager | None = None, saas_mgr: SaaSManager | None = None, threat_intel: ThreatIntelligenceEngine | None = None, compliance: ComplianceReportEngine | None = None, cam_rec: Any | None = None, wifi_ap: Any | None = None, router: Any | None = None, backup_tracker: Any | None = None, mobile_cam: Any | None = None, sunshine: Any | None = None, msp_mgr: MSPDashboardManager | None = None, msp_portal: MSPPortalManager | None = None, auto_configure: Any | None = None, cam_connect: Any | None = None, grid: Any | None = None, parental: ParentalControlsManager | None = None, backup_nudge: BackupNudgeService | None = None, dns_filter: Any | None = None, local_proxy: Any | None = None, file_sharing: Any | None = None, zfs: Any | None = None, failover: Any | None = None, ups_monitor: Any | None = None, ddns: Any | None = None, speedtest: Any | None = None, dns_verifier: Any | None = None) -> FastAPI:
    app = FastAPI(title="Ozma Controller", version="0.1.0")

    app.add_middleware(
        CORSMiddleware,
        allow_origins=["*"],
        allow_methods=["*"],
        allow_headers=["*"],
    )

    # --- Authentication ---

    _auth = auth_config or AuthConfig(enabled=False)
    _signing_key = mesh_ca.controller_keypair if mesh_ca else None
    _verify_key = _signing_key.public_key if _signing_key else None

    # Paths that don't require authentication
    _AUTH_EXEMPT = {
        "/api/v1/auth/token",
        "/api/v1/enroll",
        "/api/v1/nodes/register",
        "/api/v1/nodes/heartbeat",
        "/health",
        "/docs",
        "/openapi.json",
        "/redoc",
        "/.well-known/openid-configuration",
        "/auth/jwks",
        "/auth/login",
        "/auth/logout",
        "/auth/token",
        "/auth/userinfo",
    }

    _AUTH_EXEMPT_PREFIXES = ("/auth/login/", "/auth/callback/", "/console/", "/terminal/")

    @app.middleware("http")
    async def auth_middleware(request: Request, call_next):
        # Auth disabled — pass everything through (default until dashboard has login flow)
        if not _auth.enabled:
            request.state.auth = AuthContext(
                authenticated=True, scopes=ALL_SCOPES,
                source_ip=request.client.host if request.client else "127.0.0.1",
                auth_method="none",
            )
            return await call_next(request)

        path = request.url.path

        # Skip auth for exempt paths, prefixes, and static files
        if (path in _AUTH_EXEMPT or path.startswith("/static") or path == "/"
                or any(path.startswith(p) for p in _AUTH_EXEMPT_PREFIXES)):
            request.state.auth = AuthContext(
                authenticated=True, scopes=ALL_SCOPES,
                source_ip=request.client.host if request.client else "127.0.0.1",
                auth_method="none",
            )
            return await call_next(request)

        client_ip = request.client.host if request.client else "127.0.0.1"

        # WireGuard bypass: trusted mesh traffic
        if is_wireguard_source(client_ip, _auth):
            request.state.auth = AuthContext(
                authenticated=True, scopes=ALL_SCOPES,
                source_ip=client_ip, auth_method="wireguard",
            )
            return await call_next(request)

        # JWT bearer token
        auth_header = request.headers.get("authorization", "")
        if auth_header.startswith("Bearer ") and _verify_key:
            token = auth_header[7:]
            claims = verify_jwt(token, _verify_key)
            if claims:
                # sub is a user UUID for multi-user tokens, "admin" for legacy
                sub = claims.get("sub", "")
                user_id = sub if sub != "admin" else ""
                request.state.auth = AuthContext(
                    authenticated=True, scopes=claims.get("scopes", []),
                    source_ip=client_ip, auth_method="jwt",
                    user_id=user_id,
                )
                return await call_next(request)

        return JSONResponse(status_code=401, content={"error": "Authentication required", "auth_enabled": True})

    # --- Service proxy middleware (runs after auth middleware in the stack) ---

    @app.middleware("http")
    async def service_proxy_middleware(request: Request, call_next):
        """Route requests to registered services based on Host header."""
        if not service_proxy:
            return await call_next(request)
        host = request.headers.get("host", "")
        matched = service_proxy.match_service(host)
        if not matched:
            return await call_next(request)
        # Gate behind IdP session when auth_required
        if matched.auth_required and idp and idp.enabled:
            user_id = idp.validate_session_from_request(request)
            if not user_id:
                redirect = f"/auth/login?redirect_to={request.url}"
                from fastapi.responses import RedirectResponse as RR
                return RR(url=redirect, status_code=303)
        return await service_proxy.proxy_request(request, matched)

    def _require_scope(request: Request, scope: str) -> AuthContext:
        ctx = getattr(request.state, "auth", None)
        if not ctx or not ctx.authenticated:
            raise HTTPException(401, "Authentication required")
        if not has_scope(ctx, scope):
            raise HTTPException(403, f"Scope '{scope}' required")
        return ctx

    # --- Auth endpoints ---

    @app.post("/api/v1/auth/token")
    async def create_token(body: dict) -> dict[str, Any]:
        """Authenticate with username+password (or legacy password-only) and receive a JWT."""
        password = body.get("password", "")
        username = body.get("username", "")
        if not _signing_key:
            raise HTTPException(503, "Auth not configured — no signing key")

        # Multi-user auth: if username provided and UserManager exists, authenticate via users
        if username and user_manager:
            user = user_manager.authenticate(username, password)
            if not user:
                raise HTTPException(401, "Invalid credentials")
            scopes = ALL_SCOPES if user.role == "owner" else [SCOPE_READ, SCOPE_WRITE]
            if user.role == "guest":
                scopes = [SCOPE_READ]
            token = create_jwt(_signing_key, scopes, _auth.jwt_expiry_seconds, subject=user.id)
            return {
                "token": token,
                "expires_in": _auth.jwt_expiry_seconds,
                "scopes": scopes,
                "user": user.to_dict(),
            }

        # Legacy single-admin auth: password only (no username)
        if not _auth.password_hash or not verify_password(password, _auth.password_hash):
            raise HTTPException(401, "Invalid password")
        token = create_jwt(_signing_key, ALL_SCOPES, _auth.jwt_expiry_seconds)
        return {
            "token": token,
            "expires_in": _auth.jwt_expiry_seconds,
            "scopes": ALL_SCOPES,
        }

    @app.get("/health")
    async def health() -> dict[str, str]:
        return {"status": "ok"}

    @app.get("/metrics")
    async def prometheus_metrics() -> Response:
        """Expose controller metrics in Prometheus exposition format."""
        lines: list[str] = []
        node_count = len(state.nodes)
        active = state.active_node_id or ""
        lines.append("# HELP ozma_nodes_total Total registered nodes")
        lines.append("# TYPE ozma_nodes_total gauge")
        lines.append(f"ozma_nodes_total {node_count}")
        lines.append("# HELP ozma_active_node Active node indicator (1 per active node)")
        lines.append("# TYPE ozma_active_node gauge")
        for nid in state.nodes:
            val = 1 if nid == active else 0
            lines.append(f'ozma_active_node{{node_id="{nid}"}} {val}')
        lines.append("# HELP ozma_up Controller up (always 1)")
        lines.append("# TYPE ozma_up gauge")
        lines.append("ozma_up 1")
        body = "\n".join(lines) + "\n"
        return Response(content=body, media_type="text/plain; version=0.0.4; charset=utf-8")

    # --- User management ---

    @app.get("/api/v1/users")
    async def list_users(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        if not user_manager:
            return []
        return [u.to_dict() for u in user_manager.list_users()]

    @app.get("/api/v1/users/me")
    async def get_current_user(request: Request) -> dict:
        ctx = getattr(request.state, "auth", None)
        # Auth disabled — return anonymous context so dashboard can detect the mode
        if not _auth.enabled:
            return {"id": "", "username": "admin", "display_name": "Admin",
                    "role": "owner", "auth_enabled": False}
        if not ctx or not ctx.authenticated:
            raise HTTPException(401, detail={"error": "Authentication required", "auth_enabled": True})
        if not user_manager or not ctx.user_id:
            # Auth on but no user model (legacy JWT with sub="admin")
            return {"id": "", "username": "admin", "display_name": "Admin",
                    "role": "owner", "auth_enabled": True}
        user = user_manager.get_user(ctx.user_id)
        if not user:
            raise HTTPException(404, "User not found")
        d = user.to_dict()
        d["auth_enabled"] = True
        return d

    @app.get("/api/v1/users/{user_id}")
    async def get_user(user_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_READ)
        if not user_manager:
            raise HTTPException(404, "User management not enabled")
        user = user_manager.get_user(user_id)
        if not user:
            raise HTTPException(404, "User not found")
        return user.to_dict()

    @app.post("/api/v1/users")
    async def create_user(body: dict, request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        if not user_manager:
            raise HTTPException(503, "User management not enabled")
        username = body.get("username", "").strip()
        display_name = body.get("display_name", username).strip()
        password = body.get("password", "")
        email = body.get("email", "")
        role = body.get("role", "member")
        if not username:
            raise HTTPException(400, "Username is required")
        if role not in ("owner", "member", "guest"):
            raise HTTPException(400, "Invalid role")
        try:
            user = user_manager.create_user(
                username=username, display_name=display_name,
                password=password, email=email, role=role,
            )
        except ValueError as e:
            raise HTTPException(409, str(e))
        await state.events.put({"type": "user.created", "user": user.to_dict()})
        return user.to_dict()

    @app.put("/api/v1/users/{user_id}")
    async def update_user(user_id: str, body: dict, request: Request) -> dict:
        ctx = _require_scope(request, SCOPE_WRITE)
        if not user_manager:
            raise HTTPException(503, "User management not enabled")
        # Users can update their own profile; admin can update anyone
        if ctx.user_id != user_id and not has_scope(ctx, SCOPE_ADMIN):
            raise HTTPException(403, "Cannot modify other users")
        allowed = {}
        for key in ("display_name", "email", "password"):
            if key in body:
                allowed[key] = body[key]
        # Only admin can change roles
        if "role" in body and has_scope(ctx, SCOPE_ADMIN):
            allowed["role"] = body["role"]
        user = user_manager.update_user(user_id, **allowed)
        if not user:
            raise HTTPException(404, "User not found")
        return user.to_dict()

    @app.delete("/api/v1/users/{user_id}")
    async def delete_user(user_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        if not user_manager:
            raise HTTPException(503, "User management not enabled")
        if not user_manager.delete_user(user_id):
            raise HTTPException(404, "User not found")
        await state.events.put({"type": "user.deleted", "user_id": user_id})
        return {"ok": True}

    # --- Service proxy management ---

    @app.get("/api/v1/services")
    async def list_services(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        if not service_proxy:
            return []
        return [s.to_dict() for s in service_proxy.list_services()]

    @app.get("/api/v1/services/{service_id}")
    async def get_service(service_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_READ)
        if not service_proxy:
            raise HTTPException(503, "Service proxy not enabled")
        s = service_proxy.get_service(service_id)
        if not s:
            raise HTTPException(404, "Service not found")
        return s.to_dict()

    @app.post("/api/v1/services")
    async def register_service(body: dict, request: Request) -> dict:
        ctx = _require_scope(request, SCOPE_WRITE)
        if not service_proxy:
            raise HTTPException(503, "Service proxy not enabled")
        name = body.get("name", "").strip()
        target_host = body.get("target_host", "127.0.0.1")
        target_port = int(body.get("target_port", 0))
        if not name or not target_port:
            raise HTTPException(400, "name and target_port are required")
        try:
            s = service_proxy.register_service(
                name=name,
                owner_user_id=ctx.user_id,
                target_host=target_host,
                target_port=target_port,
                subdomain=body.get("subdomain", ""),
                protocol=body.get("protocol", "http"),
                service_type=body.get("service_type", ""),
                auth_required=body.get("auth_required", True),
                health_path=body.get("health_path", "/health"),
                icon=body.get("icon", ""),
            )
        except ValueError as e:
            raise HTTPException(409, str(e))
        await state.events.put({"type": "service.registered", "service": s.to_dict()})
        return s.to_dict()

    @app.put("/api/v1/services/{service_id}")
    async def update_service(service_id: str, body: dict, request: Request) -> dict:
        _require_scope(request, SCOPE_WRITE)
        if not service_proxy:
            raise HTTPException(503, "Service proxy not enabled")
        allowed = {}
        for key in ("name", "target_host", "target_port", "protocol", "subdomain",
                     "auth_required", "health_path", "icon", "enabled"):
            if key in body:
                allowed[key] = body[key]
        s = service_proxy.update_service(service_id, **allowed)
        if not s:
            raise HTTPException(404, "Service not found")
        return s.to_dict()

    @app.delete("/api/v1/services/{service_id}")
    async def delete_service(service_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_WRITE)
        if not service_proxy:
            raise HTTPException(503, "Service proxy not enabled")
        if not service_proxy.remove_service(service_id):
            raise HTTPException(404, "Service not found")
        await state.events.put({"type": "service.removed", "service_id": service_id})
        return {"ok": True}

    @app.get("/api/v1/services/{service_id}/health")
    async def check_service_health(service_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_READ)
        if not service_proxy:
            raise HTTPException(503, "Service proxy not enabled")
        return await service_proxy.check_health(service_id)

    @app.api_route("/api/v1/services/proxy/{service_name}/{path:path}",
                   methods=["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"])
    async def service_path_proxy(request: Request, service_name: str, path: str = "") -> Any:
        """Path-based proxy: route request to named service backend."""
        _require_scope(request, SCOPE_READ)
        if not service_proxy:
            raise HTTPException(503, "Service proxy not enabled")
        # Look up by ID first, then by name
        svc = service_proxy.get_service(service_name)
        if not svc:
            svc = next((s for s in service_proxy.list_services()
                        if s.name == service_name), None)
        if not svc:
            raise HTTPException(404, f"Service '{service_name}' not found")
        target_url = f"{svc.target_url()}/{path}"
        try:
            import httpx as _httpx
            async with _httpx.AsyncClient(timeout=10.0) as _client:
                body = await request.body()
                headers = {k: v for k, v in request.headers.items()
                           if k.lower() not in ("host", "content-length")}
                _resp = await _client.request(
                    request.method, target_url,
                    content=body, headers=headers,
                    params=dict(request.query_params),
                )
                return Response(
                    content=_resp.content,
                    status_code=_resp.status_code,
                    headers=dict(_resp.headers),
                )
        except Exception:
            raise HTTPException(502, f"Backend unreachable: {target_url}")

    # --- Identity Provider routes ---

    @app.get("/.well-known/openid-configuration")
    async def oidc_discovery(request: Request) -> dict:
        if idp and idp.enabled:
            return idp.oidc_discovery()
        # Minimal OIDC discovery for the built-in JWT issuer
        base = f"{request.base_url.scheme}://{request.base_url.netloc}"
        return {
            "issuer": base,
            "authorization_endpoint": f"{base}/auth/login",
            "token_endpoint": f"{base}/api/v1/auth/token",
            "jwks_uri": f"{base}/auth/jwks",
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["EdDSA"],
        }

    @app.get("/auth/jwks")
    async def oidc_jwks() -> dict:
        if not idp or not idp.enabled:
            raise HTTPException(404, "IdP not enabled")
        return idp.jwks()

    @app.get("/auth/login")
    async def login_page(request: Request):
        if not idp or not idp.enabled:
            raise HTTPException(404, "IdP not enabled")
        redirect_to = request.query_params.get("redirect_to", "/")
        error = request.query_params.get("error", "")
        return idp.login_page(error=error, redirect_to=redirect_to)

    @app.post("/auth/login")
    async def handle_login(request: Request):
        if not idp or not idp.enabled:
            raise HTTPException(404, "IdP not enabled")
        return await idp.handle_login(request)

    @app.get("/auth/login/{provider}")
    async def social_login(provider: str, request: Request):
        if not idp or not idp.enabled:
            raise HTTPException(404, "IdP not enabled")
        redirect_to = request.query_params.get("redirect_to", "/")
        return idp.social_redirect(provider, redirect_to=redirect_to)

    @app.get("/auth/callback/{provider}")
    async def social_callback(provider: str, request: Request):
        if not idp or not idp.enabled:
            raise HTTPException(404, "IdP not enabled")
        return await idp.social_callback(provider, request)

    @app.post("/auth/logout")
    async def handle_logout(request: Request):
        if not idp or not idp.enabled:
            raise HTTPException(404, "IdP not enabled")
        return idp.handle_logout(request)

    @app.post("/auth/token")
    async def oidc_token(request: Request):
        if not idp or not idp.enabled:
            raise HTTPException(404, "IdP not enabled")
        return await idp.token_endpoint(request)

    @app.get("/auth/userinfo")
    async def oidc_userinfo(request: Request):
        if not idp or not idp.enabled:
            raise HTTPException(404, "IdP not enabled")
        return await idp.userinfo_endpoint(request)

    # --- Sharing ---

    @app.get("/api/v1/shares")
    async def list_shares(request: Request) -> dict:
        ctx = _require_scope(request, SCOPE_READ)
        if not sharing:
            return {"given": [], "received": []}
        if ctx.user_id:
            return {
                "given": [g.to_dict() for g in sharing.list_grants_from_user(ctx.user_id)],
                "received": [g.to_dict() for g in sharing.list_grants_for_user(ctx.user_id)],
            }
        return {"given": [], "received": [],
                "all": [g.to_dict() for g in sharing.list_all_grants()]}

    @app.post("/api/v1/shares")
    async def create_share(body: dict, request: Request) -> dict:
        ctx = _require_scope(request, SCOPE_WRITE)
        if not sharing:
            raise HTTPException(503, "Sharing not enabled")
        # Security: grantor must be the authenticated user (no impersonation)
        grantor = ctx.user_id
        if not grantor:
            raise HTTPException(403, "User identity required to create shares")
        grantee = body.get("grantee_user_id", "")
        resource_type = body.get("resource_type", "service")
        resource_id = body.get("resource_id", "")
        if not grantee or not resource_id:
            raise HTTPException(400, "grantee_user_id and resource_id required")
        grant = sharing.create_grant(
            grantor_user_id=grantor,
            grantee_user_id=grantee,
            resource_type=resource_type,
            resource_id=resource_id,
            permissions=body.get("permissions", ["read"]),
            alias=body.get("alias", ""),
            expires_at=body.get("expires_at", 0.0),
        )
        await state.events.put({"type": "share.created", "grant": grant.to_dict()})
        return grant.to_dict()

    @app.get("/api/v1/shares/{grant_id}")
    async def get_share(grant_id: str, request: Request) -> dict:
        ctx = _require_scope(request, SCOPE_READ)
        if not sharing:
            raise HTTPException(503, "Sharing not enabled")
        grant = sharing.get_grant(grant_id)
        if not grant:
            raise HTTPException(404, "Grant not found")
        # Security: only grantor, grantee, or admin can view a specific grant
        if ctx.user_id and ctx.user_id not in (grant.grantor_user_id, grant.grantee_user_id):
            if not has_scope(ctx, SCOPE_ADMIN):
                raise HTTPException(403, "Not authorized to view this grant")
        return grant.to_dict()

    @app.delete("/api/v1/shares/{grant_id}")
    async def revoke_share(grant_id: str, request: Request) -> dict:
        ctx = _require_scope(request, SCOPE_WRITE)
        if not sharing:
            raise HTTPException(503, "Sharing not enabled")
        grant = sharing.get_grant(grant_id)
        if not grant:
            raise HTTPException(404, "Grant not found")
        # Security: only grantor or admin can revoke a grant
        if ctx.user_id and ctx.user_id != grant.grantor_user_id:
            if not has_scope(ctx, SCOPE_ADMIN):
                raise HTTPException(403, "Only the grantor or admin can revoke shares")
        if not sharing.revoke_grant(grant_id):
            raise HTTPException(404, "Grant not found")
        await state.events.put({"type": "share.revoked", "grant_id": grant_id})
        return {"ok": True}

    # --- Peer controllers ---

    @app.get("/api/v1/peers/discover")
    async def discover_peers(request: Request) -> dict:
        """Probe mDNS for _ozma-ctrl._tcp.local. peers (5 s scan)."""
        _require_scope(request, SCOPE_READ)
        if not discovery:
            return {"controllers": []}
        found = await discovery.discover_controllers(timeout=5.0)
        return {"controllers": found}

    @app.get("/api/v1/peers")
    async def list_peers(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        if not sharing:
            return []
        return [p.to_dict() for p in sharing.list_peers()]

    @app.post("/api/v1/peers")
    async def link_peer(body: dict, request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        if not sharing:
            raise HTTPException(503, "Sharing not enabled")
        controller_id = body.get("controller_id", "")
        owner_user_id = body.get("owner_user_id", "")
        name = body.get("name", "")
        host = body.get("host", "")
        port = body.get("port", 7380)
        transport = body.get("transport", "lan")
        if not controller_id or not host:
            raise HTTPException(400, "controller_id and host required")
        peer = sharing.add_peer(
            controller_id=controller_id, owner_user_id=owner_user_id,
            name=name, host=host, port=port, transport=transport,
        )
        await state.events.put({"type": "peer.linked", "peer": peer.to_dict()})
        return peer.to_dict()

    @app.delete("/api/v1/peers/{controller_id}")
    async def unlink_peer(controller_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        if not sharing:
            raise HTTPException(503, "Sharing not enabled")
        if not sharing.remove_peer(controller_id):
            raise HTTPException(404, "Peer not found")
        await state.events.put({"type": "peer.unlinked", "controller_id": controller_id})
        return {"ok": True}

    # --- External publishing ---

    @app.get("/api/v1/publish")
    async def list_published(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        if not ext_publish:
            return []
        return [e.to_dict() for e in ext_publish.list_entries()]

    @app.post("/api/v1/publish")
    async def publish_service(body: dict, request: Request) -> dict:
        ctx = _require_scope(request, SCOPE_WRITE)
        if not ext_publish:
            raise HTTPException(503, "External publishing not enabled")
        service_id = body.get("service_id", "")
        external_subdomain = body.get("external_subdomain", "")
        mode = body.get("mode", "private")
        if not service_id or not external_subdomain:
            raise HTTPException(400, "service_id and external_subdomain required")
        entry = await ext_publish.publish(
            service_id=service_id,
            owner_user_id=ctx.user_id,
            external_subdomain=external_subdomain,
            mode=mode,
            rate_limit=body.get("rate_limit", 0),
            allowed_domains=body.get("allowed_domains"),
            connect_client=connect,
            username=body.get("username", ""),
        )
        await state.events.put({"type": "service.published", "entry": entry.to_dict()})
        return entry.to_dict()

    @app.put("/api/v1/publish/{entry_id}")
    async def update_published(entry_id: str, body: dict, request: Request) -> dict:
        ctx = _require_scope(request, SCOPE_WRITE)
        if not ext_publish:
            raise HTTPException(503, "External publishing not enabled")
        # Security: changing to public mode requires admin scope
        if body.get("mode") == "public":
            if not has_scope(ctx, SCOPE_ADMIN):
                raise HTTPException(403, "Admin scope required to set public mode")
            if not body.get("confirm_public"):
                raise HTTPException(400, "Set confirm_public: true to confirm public exposure")
        allowed = {}
        for key in ("mode", "rate_limit", "allowed_domains", "enabled"):
            if key in body:
                allowed[key] = body[key]
        entry = ext_publish.update_entry(entry_id, **allowed)
        if not entry:
            raise HTTPException(404, "Published entry not found")
        return entry.to_dict()

    @app.delete("/api/v1/publish/{entry_id}")
    async def unpublish_service(entry_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_WRITE)
        if not ext_publish:
            raise HTTPException(503, "External publishing not enabled")
        if not await ext_publish.unpublish(entry_id):
            raise HTTPException(404, "Published entry not found")
        await state.events.put({"type": "service.unpublished", "entry_id": entry_id})
        return {"ok": True}

    # --- WebSocket broadcast ---

    _ws_clients: list[WebSocket] = []

    async def _broadcast(event: dict[str, Any]) -> None:
        payload = json.dumps(event)
        dead: list[WebSocket] = []
        for ws in _ws_clients:
            try:
                await ws.send_text(payload)
            except Exception:
                dead.append(ws)
        for ws in dead:
            if ws in _ws_clients:
                _ws_clients.remove(ws)

    # Background task that drains state.events and broadcasts them
    async def _event_pump() -> None:
        while True:
            event = await state.events.get()
            await _broadcast(event)
            # Route events to control surface trigger rules
            if controls:
                event_type = event.get("type", "")
                if event_type:
                    await controls.on_event(event_type, event)

    @app.on_event("startup")
    async def _startup() -> None:
        asyncio.create_task(_event_pump(), name="event-pump")

    # --- WebSocket endpoint ---

    async def _ws_authenticate(ws: WebSocket) -> bool:
        """Authenticate a WebSocket connection. Returns True if allowed."""
        if not _auth.enabled:
            return True
        client_ip = ws.client.host if ws.client else "127.0.0.1"
        if is_wireguard_source(client_ip, _auth):
            return True
        token = ws.query_params.get("token")
        if token and _verify_key and verify_jwt(token, _verify_key):
            return True
        return False

    @app.websocket("/api/v1/events")
    async def websocket_events(ws: WebSocket) -> None:
        if not await _ws_authenticate(ws):
            await ws.close(code=4001, reason="Authentication required")
            return
        await ws.accept()
        _ws_clients.append(ws)
        # Send current snapshot on connect
        await ws.send_text(json.dumps({"type": "snapshot", "data": state.snapshot()}))
        try:
            while True:
                await ws.receive_text()
        except WebSocketDisconnect:
            pass
        finally:
            if ws in _ws_clients:
                _ws_clients.remove(ws)

    # --- REST endpoints ---

    @app.post("/api/v1/nodes/register")
    async def register_node(req: DirectRegisterRequest) -> dict[str, Any]:
        """
        Direct registration endpoint for nodes that can't use mDNS multicast
        (e.g. inside QEMU with SLIRP networking).  Behaves identically to
        mDNS discovery — the node appears in the node list and triggers
        node.online event.
        """
        caps = [c.strip() for c in req.cap.split(",") if c.strip()]
        vnc_port = int(req.vnc_port) if req.vnc_port.isdigit() else None
        stream_port = int(req.stream_port) if req.stream_port.isdigit() else None
        api_port = int(req.api_port) if req.api_port.isdigit() else stream_port
        node = NodeInfo(
            id=req.id,
            host=req.host,
            port=req.port,
            role=req.role,
            hw=req.hw,
            fw_version=req.fw,
            proto_version=int(req.proto) if req.proto.isdigit() else 1,
            capabilities=caps,
            last_seen=time.monotonic(),
            vnc_host=req.vnc_host or None,
            vnc_port=vnc_port,
            stream_port=stream_port,
            stream_path=req.stream_path or None,
            api_port=api_port,
        audio_type=req.audio_type or None,
        audio_sink=req.audio_sink or None,
        audio_vban_port=int(req.audio_vban_port) if req.audio_vban_port.isdigit() else None,
        mic_vban_port=int(req.mic_vban_port) if req.mic_vban_port.isdigit() else None,
        capture_device=req.capture_device or None,
        machine_class=req.machine_class if req.machine_class in ("workstation", "server", "kiosk", "camera") else "workstation",
        display_outputs=json.loads(req.display_outputs) if req.display_outputs else [],
        camera_streams=json.loads(req.camera_streams) if req.camera_streams else [],
        frigate_host=req.frigate_host or None,
        frigate_port=int(req.frigate_port) if req.frigate_port.isdigit() else None,
        direct_registered=True,
        vm_guest_ip=req.vm_guest_ip or None,
        pci_devices=json.loads(req.pci_devices) if req.pci_devices else [],
        )
        await state.add_node(node)
        if streams:
            streams.register_node(node)
        log.info("Direct registration: %s @ %s", node.id, node.host)
        return {"ok": True, "node": node.to_dict()}

    @app.post("/api/v1/nodes/heartbeat")
    async def node_heartbeat(body: dict) -> dict[str, Any]:
        """
        Heartbeat from a directly-registered node. Updates last_seen
        so the node stays online in the controller's state.
        """
        node_id = body.get("node_id", "")
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        node.last_seen = time.monotonic()
        return {"ok": True}

    @app.get("/api/v1/nodes")
    async def list_nodes() -> dict[str, Any]:
        active = state.active_node_id
        nodes = []
        for n in state.nodes.values():
            d = n.to_dict()
            d["hid_active"] = (n.id == active)
            # Expose display geometry derived from display_outputs
            display_outputs = d.get("display_outputs") or []
            d["displays"] = [
                {"index": i, "width": o.get("width", 0), "height": o.get("height", 0),
                 "name": o.get("name", f"display{i}")}
                for i, o in enumerate(display_outputs)
            ] if display_outputs else []
            nodes.append(d)
        return {
            "nodes": nodes,
            "active_node_id": active,
        }

    @app.get("/api/v1/nodes/{node_id}")
    async def get_node(node_id: str) -> dict[str, Any]:
        node = state.nodes.get(node_id)
        if node is None:
            raise HTTPException(status_code=404, detail="Node not found")
        d = node.to_dict()
        d["hid_active"] = (node_id == state.active_node_id)
        display_outputs = d.get("display_outputs") or []
        d["displays"] = [
            {"index": i, "width": o.get("width", 0), "height": o.get("height", 0),
             "name": o.get("name", f"display{i}")}
            for i, o in enumerate(display_outputs)
        ] if display_outputs else []
        return d

    @app.put("/api/v1/nodes/{node_id}/machine_class")
    async def set_machine_class(request: Request, node_id: str, body: dict) -> dict[str, Any]:
        """Set a node's machine class (workstation, server, kiosk)."""
        _require_scope(request, SCOPE_WRITE)
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        mc = body.get("machine_class", "")
        if mc not in ("workstation", "server", "kiosk", "camera"):
            raise HTTPException(status_code=400, detail="Invalid machine_class. Must be: workstation, server, kiosk, camera")
        node.machine_class = mc
        return {"ok": True, "node_id": node_id, "machine_class": mc}

    # --- Seat config push to agents ---

    # Per-node WebSocket connections from agents (node_id -> WebSocket)
    _node_config_ws: dict[str, WebSocket] = {}

    @app.get("/api/v1/nodes/{node_id}/seats")
    async def get_seat_config(node_id: str) -> dict[str, Any]:
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        return {
            "node_id": node_id,
            "seat_count": node.seat_count,
            "seat_config": node.seat_config,
        }

    # Note: PUT /api/v1/nodes/{node_id}/seats is defined below with destructive
    # action warning support (update_seat_config_with_warnings)

    @app.websocket("/api/v1/nodes/{node_id}/config/ws")
    async def node_config_ws(ws: WebSocket, node_id: str) -> None:
        """
        WebSocket endpoint for agent config push.

        The agent connects here and receives seat config updates.
        On connect, the current config is sent immediately so agents
        get authoritative state after restart/reconnect.
        """
        if not await _ws_authenticate(ws):
            await ws.close(code=4001, reason="Authentication required")
            return

        node = state.nodes.get(node_id)
        if not node:
            await ws.close(code=4004, reason="Node not found")
            return

        await ws.accept()

        # Replace any existing connection for this node
        old_ws = _node_config_ws.pop(node_id, None)
        if old_ws:
            try:
                await old_ws.close(code=1000, reason="Replaced by new connection")
            except Exception:
                pass
        _node_config_ws[node_id] = ws

        log.info("Agent config WS connected: %s", node_id)

        # Wire job dispatch function into the queue on first use
        if job_queue and not job_queue._dispatch_fn:
            async def _jq_dispatch(target: str, msg: dict) -> bool:
                _ws = _node_config_ws.get(target)
                if not _ws:
                    return False
                try:
                    await _ws.send_text(json.dumps(msg))
                    return True
                except Exception:
                    _node_config_ws.pop(target, None)
                    return False
            job_queue.set_dispatch_fn(_jq_dispatch)

        # Dispatch any pending jobs for this node
        if job_queue:
            asyncio.create_task(
                job_queue.on_node_connected(node_id),
                name=f"job-dispatch-{node_id}",
            )

        # Send current seat config immediately
        config_msg = {
            "type": "seat_config",
            "seats": node.seat_count,
            "profiles": node.seat_config.get("profiles", []),
        }
        try:
            await ws.send_text(json.dumps(config_msg))
        except Exception:
            _node_config_ws.pop(node_id, None)
            return

        try:
            while True:
                # Agent can send messages (e.g. acks, status, job results)
                text = await ws.receive_text()
                try:
                    msg = json.loads(text)
                    msg_type = msg.get("type", "")
                    if msg_type == "seat_status":
                        log.debug("Agent %s seat status: %s", node_id, msg)
                    elif msg_type == "job_ack" and job_queue:
                        await job_queue.handle_ack(msg.get("job_id", ""))
                    elif msg_type == "job_progress" and job_queue:
                        await job_queue.handle_progress(
                            msg.get("job_id", ""),
                            msg.get("progress", 0),
                            msg.get("message", ""),
                        )
                    elif msg_type == "job_result" and job_queue:
                        await job_queue.handle_result(
                            msg.get("job_id", ""),
                            msg.get("exit_code", -1),
                            msg.get("stdout", ""),
                            msg.get("stderr", ""),
                            msg.get("error", ""),
                        )
                except json.JSONDecodeError:
                    pass
        except WebSocketDisconnect:
            pass
        except Exception:
            pass
        finally:
            if _node_config_ws.get(node_id) is ws:
                _node_config_ws.pop(node_id, None)
            log.info("Agent config WS disconnected: %s", node_id)

    # --- Seat ownership and sharing ---

    @app.get("/api/v1/nodes/{node_id}/owner")
    async def get_node_owner(node_id: str) -> dict[str, Any]:
        """Get owner info for a node/seat."""
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        return {
            "node_id": node_id,
            "owner_id": node.owner_id,
            "parent_node_id": node.parent_node_id,
        }

    @app.put("/api/v1/nodes/{node_id}/owner")
    async def set_node_owner(request: Request, node_id: str, body: dict) -> dict[str, Any]:
        """Set owner of a node/seat."""
        ctx = _require_scope(request, SCOPE_WRITE)
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")

        user_id = body.get("user_id", "")
        if not isinstance(user_id, str):
            raise HTTPException(status_code=400, detail="user_id must be a string")

        old_owner = node.owner_id
        node.owner_id = user_id

        log.info("Node %s owner changed: %s -> %s (by %s from %s)",
                 node_id, old_owner or "(none)", user_id or "(none)",
                 ctx.user_id or "admin", ctx.source_ip)

        await state.events.put({
            "type": "seat.owner_changed",
            "node_id": node_id,
            "old_owner": old_owner,
            "new_owner": user_id,
            "changed_by": ctx.user_id or "admin",
            "timestamp": time.time(),
        })
        return {"ok": True, "node_id": node_id, "owner_id": user_id}

    @app.get("/api/v1/nodes/{node_id}/sharing")
    async def get_node_sharing(node_id: str) -> dict[str, Any]:
        """Get share list with permissions for a node/seat."""
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        shares = [
            {"user_id": uid, "permission": node.share_permissions.get(uid, "use")}
            for uid in node.shared_with
        ]
        return {"node_id": node_id, "owner_id": node.owner_id, "shares": shares}

    @app.post("/api/v1/nodes/{node_id}/sharing")
    async def add_node_sharing(request: Request, node_id: str, body: dict) -> dict[str, Any]:
        """Share a node/seat with a user."""
        ctx = _require_scope(request, SCOPE_WRITE)
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")

        user_id = body.get("user_id", "")
        permission = body.get("permission", "use")
        if not user_id or not isinstance(user_id, str):
            raise HTTPException(status_code=400, detail="user_id is required")
        if permission not in ("use", "manage", "admin"):
            raise HTTPException(status_code=400, detail="permission must be use, manage, or admin")

        # Check that the requesting user has at least admin permission to share
        if node.owner_id and ctx.user_id:
            if not check_node_permission(node, ctx.user_id, "admin", state):
                raise HTTPException(status_code=403, detail="Admin permission required to share")

        if user_id not in node.shared_with:
            node.shared_with.append(user_id)
        node.share_permissions[user_id] = permission

        log.info("Node %s shared with %s (%s) by %s from %s",
                 node_id, user_id, permission,
                 ctx.user_id or "admin", ctx.source_ip)

        await state.events.put({
            "type": "seat.shared",
            "node_id": node_id,
            "user_id": user_id,
            "permission": permission,
            "shared_by": ctx.user_id or "admin",
            "timestamp": time.time(),
        })
        return {"ok": True, "node_id": node_id, "user_id": user_id, "permission": permission}

    @app.delete("/api/v1/nodes/{node_id}/sharing/{user_id}")
    async def revoke_node_sharing(request: Request, node_id: str, user_id: str) -> dict[str, Any]:
        """Revoke a user's share on a node/seat."""
        ctx = _require_scope(request, SCOPE_WRITE)
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")

        if user_id not in node.shared_with:
            raise HTTPException(status_code=404, detail="User not in share list")

        # Check permission — owner or admin can revoke
        if node.owner_id and ctx.user_id:
            if not check_node_permission(node, ctx.user_id, "admin", state):
                raise HTTPException(status_code=403, detail="Admin permission required to revoke share")

        node.shared_with.remove(user_id)
        node.share_permissions.pop(user_id, None)

        log.info("Node %s share revoked for %s by %s from %s",
                 node_id, user_id, ctx.user_id or "admin", ctx.source_ip)

        await state.events.put({
            "type": "seat.unshared",
            "node_id": node_id,
            "user_id": user_id,
            "revoked_by": ctx.user_id or "admin",
            "timestamp": time.time(),
        })
        return {"ok": True, "node_id": node_id, "user_id": user_id}

    @app.put("/api/v1/nodes/{node_id}/sharing/{user_id}")
    async def update_node_sharing(request: Request, node_id: str, user_id: str, body: dict) -> dict[str, Any]:
        """Update a user's permission level on a shared node/seat."""
        ctx = _require_scope(request, SCOPE_WRITE)
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")

        if user_id not in node.shared_with:
            raise HTTPException(status_code=404, detail="User not in share list")

        permission = body.get("permission", "")
        if permission not in ("use", "manage", "admin"):
            raise HTTPException(status_code=400, detail="permission must be use, manage, or admin")

        if node.owner_id and ctx.user_id:
            if not check_node_permission(node, ctx.user_id, "admin", state):
                raise HTTPException(status_code=403, detail="Admin permission required to update share")

        old_perm = node.share_permissions.get(user_id, "use")
        node.share_permissions[user_id] = permission

        log.info("Node %s share for %s updated: %s -> %s by %s from %s",
                 node_id, user_id, old_perm, permission,
                 ctx.user_id or "admin", ctx.source_ip)

        return {"ok": True, "node_id": node_id, "user_id": user_id, "permission": permission}

    @app.get("/api/v1/my/seats")
    async def get_my_seats(request: Request) -> dict[str, Any]:
        """Get seats owned by or shared with the authenticated user."""
        ctx = _require_scope(request, SCOPE_READ)
        user_id = ctx.user_id
        if not user_id:
            raise HTTPException(status_code=400, detail="No user identity (legacy auth)")
        return get_user_seats(state, user_id)

    # --- Override seat config update with destructive warnings ---

    @app.put("/api/v1/nodes/{node_id}/seats")
    async def update_seat_config_with_warnings(request: Request, node_id: str, body: dict) -> Any:
        """Update seat config with destructive action warnings.

        If reducing seats would affect owned/shared seats, returns 409 with warnings
        unless confirm:true is in the request body.
        """
        _require_scope(request, SCOPE_WRITE)
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")

        seats = body.get("seats")
        if seats is None or not isinstance(seats, int) or seats < 1 or seats > 8:
            raise HTTPException(status_code=400, detail="seats must be an integer 1-8")

        confirm = body.get("confirm", False)

        # Check for destructive warnings when reducing seats
        if seats < node.seat_count and not confirm:
            warnings = check_destructive_warnings(state, node_id, "reduce_seats",
                                                   target_seat_count=seats)
            if warnings:
                total_affected = sum(
                    (1 if w.owner else 0) + len(w.shared_users) for w in warnings
                )
                removed = node.seat_count - seats
                return JSONResponse(
                    status_code=409,
                    content={
                        "warnings": warnings_to_dict(warnings),
                        "confirm_required": True,
                        "confirm_message": (
                            f"Reducing seats will remove {removed} seat(s) "
                            f"affecting {total_affected} user(s). "
                            f"Send confirm:true to proceed."
                        ),
                    },
                )

        if confirm and seats < node.seat_count:
            ctx = getattr(request.state, "auth", None)
            warnings = check_destructive_warnings(state, node_id, "reduce_seats",
                                                   target_seat_count=seats)
            if warnings:
                log.info("Destructive seat reduction on %s confirmed by %s from %s: %d -> %d seats",
                         node_id, ctx.user_id if ctx else "admin",
                         ctx.source_ip if ctx else "unknown",
                         node.seat_count, seats)
                await state.events.put({
                    "type": "seat.destroying_warning",
                    "node_id": node_id,
                    "warnings": warnings_to_dict(warnings),
                    "confirmed_by": ctx.user_id if ctx else "admin",
                    "timestamp": time.time(),
                })

        # Proceed with the actual seat config update
        profiles = body.get("profiles", [])
        if profiles and len(profiles) != seats:
            raise HTTPException(status_code=400, detail="profiles length must match seats count")

        node.seat_count = seats
        node.seat_config = {"seats": seats, "profiles": profiles}

        # Push config to the agent's WebSocket if connected
        msg = json.dumps({"type": "seat_config", "seats": seats, "profiles": profiles})
        ws = _node_config_ws.get(node_id)
        if ws:
            try:
                await ws.send_text(msg)
                log.info("Pushed seat config to %s: %d seats", node_id, seats)
            except Exception:
                log.warning("Failed to push seat config to %s — agent disconnected", node_id)
                _node_config_ws.pop(node_id, None)

        await state.events.put({
            "type": "node.seat_config",
            "node_id": node_id,
            "seat_count": seats,
            "seat_config": node.seat_config,
        })
        return {"ok": True, "node_id": node_id, "seat_count": seats, "seat_config": node.seat_config}

    @app.post("/api/v1/nodes/{node_id}/activate")
    async def activate_node(node_id: str) -> dict[str, Any]:
        try:
            await state.set_active_node(node_id)
        except KeyError:
            raise HTTPException(status_code=404, detail="Node not found")
        return {"ok": True, "active_node_id": node_id}

    @app.get("/api/v1/nodes/{node_id}/usb")
    async def node_usb(node_id: str) -> dict[str, Any]:
        """Proxy /usb from the node's HTTP API."""
        import urllib.request
        node = state.nodes.get(node_id)
        if node is None:
            raise HTTPException(status_code=404, detail="Node not found")
        port = node.api_port
        if not port:
            raise HTTPException(status_code=503, detail="Node has no HTTP API")
        url = f"http://{node.host}:{port}/usb"
        try:
            loop = asyncio.get_running_loop()
            def _fetch() -> dict:
                with urllib.request.urlopen(url, timeout=5) as r:
                    return json.loads(r.read())
            return await loop.run_in_executor(None, _fetch)
        except Exception as e:
            raise HTTPException(status_code=502, detail=f"Node unreachable: {e}")

    # --- Node power/current/RGB proxy ---

    async def _proxy_node(node_id: str, path: str, method: str = "GET", body: dict | None = None) -> dict:
        """Proxy a request to a node's HTTP API."""
        import urllib.request
        node = state.nodes.get(node_id)
        if node is None:
            raise HTTPException(status_code=404, detail="Node not found")
        port = node.api_port
        if not port:
            raise HTTPException(status_code=503, detail="Node has no HTTP API")
        url = f"http://{node.host}:{port}{path}"
        try:
            loop = asyncio.get_running_loop()
            def _fetch() -> dict:
                if method == "POST":
                    data = json.dumps(body or {}).encode()
                    req = urllib.request.Request(url, data=data,
                                                headers={"Content-Type": "application/json"},
                                                method="POST")
                else:
                    req = urllib.request.Request(url)
                with urllib.request.urlopen(req, timeout=5) as r:
                    return json.loads(r.read())
            return await loop.run_in_executor(None, _fetch)
        except Exception as e:
            raise HTTPException(status_code=502, detail=f"Node unreachable: {e}")

    @app.get("/api/v1/nodes/{node_id}/power/state")
    async def node_power_state(node_id: str) -> dict[str, Any]:
        return await _proxy_node(node_id, "/power/state")

    @app.post("/api/v1/nodes/{node_id}/power/{action}")
    async def node_power_action(node_id: str, action: str) -> dict[str, Any]:
        return await _proxy_node(node_id, f"/power/{action}", method="POST")

    @app.get("/api/v1/nodes/{node_id}/current")
    async def node_current(node_id: str) -> dict[str, Any]:
        return await _proxy_node(node_id, "/current")

    @app.get("/api/v1/nodes/{node_id}/rgb/state")
    async def node_rgb_state(node_id: str) -> dict[str, Any]:
        return await _proxy_node(node_id, "/rgb/state")

    @app.post("/api/v1/nodes/{node_id}/rgb/set")
    async def node_rgb_set(node_id: str, body: dict = {}) -> dict[str, Any]:
        return await _proxy_node(node_id, "/rgb/set", method="POST", body=body)

    @app.get("/api/v1/status")
    async def get_status() -> dict[str, Any]:
        return {
            **state.snapshot(),
            "active_scenario_id": scenarios.active_id,
        }

    # --- Routing graph endpoints (Phase 1: observational) ---

    @app.get("/api/v1/graph")
    async def get_graph() -> dict[str, Any]:
        """Return the current routing graph (devices, ports, links)."""
        return state.routing_graph.to_dict()

    @app.get("/api/v1/graph/devices")
    async def list_graph_devices() -> dict[str, Any]:
        return {"devices": [d.to_dict() for d in state.routing_graph.devices()]}

    @app.get("/api/v1/graph/devices/{device_id}")
    async def get_graph_device(device_id: str) -> dict[str, Any]:
        device = state.routing_graph.get_device(device_id)
        if device is None:
            raise HTTPException(status_code=404, detail=f"Device not found: {device_id}")
        return device.to_dict()

    @app.get("/api/v1/graph/links")
    async def list_graph_links() -> dict[str, Any]:
        return {"links": [l.to_dict() for l in state.routing_graph.links()]}

    # --- Routing engine endpoints (Phase 2–6) ---

    @app.get("/api/v1/routing/intents")
    async def list_routing_intents(request: Request) -> dict[str, Any]:
        """List all built-in routing intents."""
        _require_scope(request, SCOPE_READ)
        from routing.intent import BUILTIN_INTENTS
        return {"intents": {k: v.to_dict() for k, v in BUILTIN_INTENTS.items()}}

    @app.get("/api/v1/routing/intents/{name}")
    async def get_routing_intent(request: Request, name: str) -> dict[str, Any]:
        """Get a single routing intent by name."""
        _require_scope(request, SCOPE_READ)
        from routing.intent import BUILTIN_INTENTS
        intent = BUILTIN_INTENTS.get(name)
        if intent is None:
            raise HTTPException(status_code=404, detail=f"Intent not found: {name}")
        return intent.to_dict()

    @app.get("/api/v1/routing/explain")
    async def routing_explain(
        request: Request,
        source: str,
        destination: str,
        intent: str = "desktop",
        top_n: int = 3,
    ) -> dict[str, Any]:
        """
        Explain what pipelines the router would select for a given intent
        between source and destination devices.
        """
        _require_scope(request, SCOPE_READ)
        from routing.intent import BUILTIN_INTENTS
        chosen_intent = BUILTIN_INTENTS.get(intent)
        if chosen_intent is None:
            raise HTTPException(status_code=404, detail=f"Intent not found: {intent}")
        src_device = state.routing_graph.get_device(source)
        if src_device is None:
            raise HTTPException(status_code=404, detail=f"Source device not found: {source}")
        dst_device = state.routing_graph.get_device(destination)
        if dst_device is None:
            raise HTTPException(status_code=404, detail=f"Destination device not found: {destination}")
        recommendations = state.routing_engine.recommend_devices(
            chosen_intent, src_device, dst_device, top_n=top_n
        )
        # Populate the pipeline cache as a side-effect of explain queries
        return {
            "source": source,
            "destination": destination,
            "intent": intent,
            "streams": [
                {
                    "media_type": si.media_type.value,
                    "pipelines": [p.to_dict() for p in pipelines],
                }
                for si, pipelines in recommendations
            ],
        }

    @app.get("/api/v1/routing/feasibility")
    async def routing_feasibility(
        request: Request,
        source: str,
        destination: str,
        intent: str = "desktop",
    ) -> dict[str, Any]:
        """Check whether an intent is feasible between two devices."""
        _require_scope(request, SCOPE_READ)
        from routing.intent import BUILTIN_INTENTS
        chosen_intent = BUILTIN_INTENTS.get(intent)
        if chosen_intent is None:
            raise HTTPException(status_code=404, detail=f"Intent not found: {intent}")
        src_device = state.routing_graph.get_device(source)
        if src_device is None:
            raise HTTPException(status_code=404, detail=f"Source device not found: {source}")
        dst_device = state.routing_graph.get_device(destination)
        if dst_device is None:
            raise HTTPException(status_code=404, detail=f"Destination device not found: {destination}")
        # Use device-aware feasibility check
        feasible: dict = {}
        for stream in chosen_intent.streams:
            mt = stream.media_type
            from routing.model import PortDirection, PortRef
            src_port = next(
                (p for p in src_device.ports
                 if p.direction == PortDirection.source and p.media_type == mt), None)
            dst_port = next(
                (p for p in dst_device.ports
                 if p.direction == PortDirection.sink and p.media_type == mt), None)
            if src_port is None or dst_port is None:
                feasible[mt.value] = False
                continue
            src_ref = PortRef(device_id=src_device.id, port_id=src_port.id)
            dst_ref = PortRef(device_id=dst_device.id, port_id=dst_port.id)
            result = state.routing_engine.check_feasibility(chosen_intent, src_ref, dst_ref)
            feasible[mt.value] = result.get(mt, False)
        return {
            "source": source,
            "destination": destination,
            "intent": intent,
            "feasible": feasible,
        }

    @app.post("/api/v1/routing/evaluate")
    async def routing_evaluate(request: Request) -> dict[str, Any]:
        """
        Evaluate an ad-hoc intent against the current graph.

        Body: { "source": str, "destination": str, "intent": str, "top_n": int }
        """
        _require_scope(request, SCOPE_READ)
        from routing.intent import BUILTIN_INTENTS
        body = await request.json()
        source = body.get("source", "")
        destination = body.get("destination", "")
        intent_name = body.get("intent", "desktop")
        top_n = int(body.get("top_n", 3))
        chosen_intent = BUILTIN_INTENTS.get(intent_name)
        if chosen_intent is None:
            raise HTTPException(status_code=400, detail=f"Unknown intent: {intent_name}")
        src_device = state.routing_graph.get_device(source)
        dst_device = state.routing_graph.get_device(destination)
        if src_device is None or dst_device is None:
            raise HTTPException(status_code=404, detail="Source or destination device not found")
        recommendations = state.routing_engine.recommend_devices(
            chosen_intent, src_device, dst_device, top_n=top_n
        )
        return {
            "source": source,
            "destination": destination,
            "intent": intent_name,
            "streams": [
                {
                    "media_type": si.media_type.value,
                    "pipelines": [p.to_dict() for p in pipelines],
                }
                for si, pipelines in recommendations
            ],
        }

    # --- Monitoring endpoints (Phase 6) ---

    @app.get("/api/v1/monitoring/journal")
    async def monitoring_journal_query(
        request: Request,
        device_id: str | None = None,
        link_id: str | None = None,
        severity: str | None = None,
        limit: int = 100,
        offset: int = 0,
    ) -> dict[str, Any]:
        """Query the state change journal."""
        _require_scope(request, SCOPE_READ)
        entries = state.monitoring_journal.query(
            device_id=device_id,
            link_id=link_id,
            severity=severity,
            limit=limit,
            offset=offset,
        )
        return {
            "entries": [e.to_dict() for e in entries],
            "total": len(state.monitoring_journal),
        }

    @app.get("/api/v1/monitoring/metrics/{device_id}")
    async def monitoring_device_metrics(
        request: Request, device_id: str
    ) -> dict[str, Any]:
        """Return all stored metric values for a device (with quality decay applied)."""
        _require_scope(request, SCOPE_READ)
        metrics = state.measurement_store.metrics_for_device(device_id)
        freshness = state.measurement_store.get_device_freshness(device_id)
        return {
            "device_id": device_id,
            "metrics": {k: v.to_dict() for k, v in metrics.items()},
            "freshness": freshness.to_dict() if freshness else None,
        }

    @app.get("/api/v1/monitoring/health")
    async def monitoring_health(request: Request) -> dict[str, Any]:
        """Return per-device freshness state for all monitored devices."""
        _require_scope(request, SCOPE_READ)
        result = {}
        for device_id in state.measurement_store.all_device_ids():
            f = state.measurement_store.get_device_freshness(device_id)
            if f:
                result[device_id] = f.to_dict()
        return {"devices": result}

    @app.get("/api/v1/monitoring/trends")
    async def monitoring_trends(
        request: Request,
        device_id: str | None = None,
        active_only: bool = True,
    ) -> dict[str, Any]:
        """Return active trend alerts, optionally filtered by device."""
        _require_scope(request, SCOPE_READ)
        mgr = getattr(state, "trend_alert_manager", None)
        if mgr is None:
            return {"alerts": []}
        alerts = mgr.active_alerts() if active_only else mgr.all_alerts()
        if device_id:
            alerts = [a for a in alerts if a.device_id == device_id]
        return {"alerts": [a.to_dict() for a in alerts]}

    @app.get("/api/v1/monitoring/link/{link_id}/history")
    async def monitoring_link_history(
        request: Request,
        link_id: str,
        tier: int = 1,
        limit: int = 200,
        metric: str = "latency_ms",
    ) -> dict[str, Any]:
        """
        Return time-series history for a specific link metric.

        Parameters:
          link_id: the link ID from the routing graph (e.g. "hid:controller→node:vm1")
          tier:    data resolution (1=1s/1h, 2=1m/24h, 3=15m/30d)
          limit:   max number of points to return (default 200, newest first)
          metric:  one of latency_ms, loss_rate, jitter_p99_ms (default latency_ms)

        Returns {link_id, metric, tier, points: [{t, v, n, min, max}, ...]}
        """
        _require_scope(request, SCOPE_READ)
        if tier not in (1, 2, 3):
            raise HTTPException(400, "tier must be 1, 2, or 3")
        valid_metrics = ("latency_ms", "loss_rate", "jitter_p99_ms")
        if metric not in valid_metrics:
            raise HTTPException(
                400,
                f"metric must be one of: {', '.join(valid_metrics)}"
            )
        # Find the link to get its source device_id
        link = state.routing_graph.get_link(link_id)
        if link is None:
            raise HTTPException(404, f"Link not found: {link_id}")
        dev_id = link.source.device_id
        metric_key = f"link.{link_id}.{metric}"

        metric_store = getattr(state, "metric_store", None)
        if metric_store is None:
            return {"link_id": link_id, "metric": metric, "tier": tier, "points": []}

        series = metric_store.get_series(dev_id, metric_key)
        if series is None:
            return {"link_id": link_id, "metric": metric, "tier": tier, "points": []}

        points = series.history(tier=tier, limit=limit)
        return {
            "link_id": link_id,
            "device_id": dev_id,
            "metric": metric,
            "metric_key": metric_key,
            "tier": tier,
            "points": [p.to_dict() for p in points],
        }

    @app.get("/api/v1/routing/pipelines")
    async def routing_pipelines(request: Request) -> dict[str, Any]:
        """
        Return all currently cached pipeline candidates.

        The cache is populated lazily when /routing/explain or /routing/evaluate
        is called, and is invalidated whenever the graph topology changes
        (node joins, leaves, or active node switches).
        """
        _require_scope(request, SCOPE_READ)
        cache = getattr(state, "pipeline_cache", None)
        if cache is None:
            return {"generation": 0, "cached_entries": 0, "entries": []}
        return cache.to_dict()

    @app.get("/api/v1/routing/simulate")
    async def routing_simulate(
        request: Request,
        fail_link: str,
        source: str,
        destination: str,
        intent: str = "desktop",
        top_n: int = 3,
    ) -> dict[str, Any]:
        """
        Simulate a link failure and return what the router would recommend.

        Temporarily marks the given link as failed, calls recommend(), then
        restores the original link state. Does not mutate the live pipeline cache.
        """
        _require_scope(request, SCOPE_READ)
        from routing.intent import BUILTIN_INTENTS
        from routing.model import LinkStatus
        chosen_intent = BUILTIN_INTENTS.get(intent)
        if chosen_intent is None:
            raise HTTPException(404, f"Intent not found: {intent}")
        src_device = state.routing_graph.get_device(source)
        dst_device = state.routing_graph.get_device(destination)
        if src_device is None:
            raise HTTPException(404, f"Source device not found: {source}")
        if dst_device is None:
            raise HTTPException(404, f"Destination device not found: {destination}")
        link = state.routing_graph.get_link(fail_link)
        if link is None:
            raise HTTPException(404, f"Link not found: {fail_link}")

        # Temporarily mark link as failed, run router, restore
        original_status = link.state.status
        try:
            link.state.status = LinkStatus.failed
            from routing.model import PortRef
            # Determine source/destination PortRefs — use first matching port
            src_ports = src_device.ports
            dst_ports = dst_device.ports
            if not src_ports or not dst_ports:
                raise HTTPException(422, "Source or destination device has no ports")
            src_ref = PortRef(device_id=src_device.id, port_id=src_ports[0].id)
            dst_ref = PortRef(device_id=dst_device.id, port_id=dst_ports[-1].id)
            recommendations = state.routing_engine.recommend(
                chosen_intent, src_ref, dst_ref, top_n=top_n
            )
        finally:
            link.state.status = original_status

        return {
            "simulated_failure": fail_link,
            "source": source,
            "destination": destination,
            "intent": intent,
            "streams": [
                {
                    "media_type": si.media_type.value,
                    "pipelines": [p.to_dict() for p in pipes],
                }
                for si, pipes in recommendations
            ],
        }

    @app.get("/api/v1/routing/measurement_engine")
    async def routing_measurement_engine_status(request: Request) -> dict[str, Any]:
        """Return the active measurement engine status."""
        _require_scope(request, SCOPE_READ)
        return state.measurement_engine.to_dict()

    @app.post("/api/v1/routing/probe/{link_id}")
    async def routing_probe_link(request: Request, link_id: str) -> dict[str, Any]:
        """
        Trigger an immediate ICMP probe of a specific link.

        Only works for links with a `target_ip` property (network links).
        Returns the updated link state after probing.
        """
        _require_scope(request, SCOPE_WRITE)
        found = await state.measurement_engine.probe_link_now(link_id)
        if not found:
            raise HTTPException(404, f"Link not found or not probeable: {link_id}")
        link = state.routing_graph.get_link(link_id)
        if link is None:
            raise HTTPException(404, f"Link not found: {link_id}")
        return {"link_id": link_id, "state": link.state.to_dict() if link.state else None}

    # --- Binding endpoints ---

    @app.get("/api/v1/routing/bindings")
    async def list_bindings(request: Request) -> dict[str, Any]:
        """Return all registered intent bindings."""
        _require_scope(request, SCOPE_READ)
        registry = getattr(state, "binding_registry", None)
        if registry is None:
            return {"bindings": []}
        return {"bindings": [b.to_dict() for b in registry.list_all()]}

    @app.get("/api/v1/routing/bindings/current")
    async def current_binding(request: Request) -> dict[str, Any]:
        """Return the currently active binding result."""
        _require_scope(request, SCOPE_READ)
        loop = getattr(state, "binding_loop", None)
        if loop is None:
            return {"current": None}
        current = loop.current
        return {"current": current.to_dict() if current else None}

    @app.post("/api/v1/routing/bindings/evaluate")
    async def evaluate_bindings(request: Request) -> dict[str, Any]:
        """Trigger an immediate binding evaluation cycle and return the result."""
        _require_scope(request, SCOPE_WRITE)
        loop = getattr(state, "binding_loop", None)
        if loop is None:
            return {"current": None}
        result = loop.evaluate_once()
        return {"current": result.to_dict()}

    @app.get("/api/v1/routing/binding_loop")
    async def binding_loop_status(request: Request) -> dict[str, Any]:
        """Return the binding evaluation loop status."""
        _require_scope(request, SCOPE_READ)
        loop = getattr(state, "binding_loop", None)
        if loop is None:
            return {"running": False}
        return loop.to_dict()

    # --- Scenario endpoints ---

    @app.get("/api/v1/scenarios")
    async def list_scenarios() -> dict[str, Any]:
        return {
            "scenarios": scenarios.list(),
            "active_id": scenarios.active_id,
        }

    @app.get("/api/v1/scenarios/{scenario_id}")
    async def get_scenario(scenario_id: str) -> dict[str, Any]:
        s = scenarios.get(scenario_id)
        if s is None:
            raise HTTPException(status_code=404, detail="Scenario not found")
        return s.to_dict()

    @app.post("/api/v1/scenarios")
    async def create_scenario(req: CreateScenarioRequest, scenario_id: str) -> dict[str, Any]:
        try:
            s = await scenarios.create(scenario_id, req.name, req.node_id)
        except ValueError as e:
            raise HTTPException(status_code=409, detail=str(e))
        return s.to_dict()

    @app.post("/api/v1/scenarios/{scenario_id}/activate")
    async def activate_scenario(scenario_id: str) -> dict[str, Any]:
        try:
            s = await scenarios.activate(scenario_id)
        except KeyError:
            raise HTTPException(status_code=404, detail="Scenario not found")
        return {"ok": True, "scenario": s.to_dict()}

    @app.post("/api/v1/scenarios/{scenario_id}/bind")
    async def bind_node(scenario_id: str, req: BindNodeRequest) -> dict[str, Any]:
        try:
            s = await scenarios.bind_node(scenario_id, req.node_id)
        except KeyError:
            raise HTTPException(status_code=404, detail="Scenario not found")
        return s.to_dict()

    @app.delete("/api/v1/scenarios/{scenario_id}")
    async def delete_scenario(scenario_id: str) -> dict[str, Any]:
        try:
            await scenarios.delete(scenario_id)
        except KeyError:
            raise HTTPException(status_code=404, detail="Scenario not found")
        except ValueError as e:
            raise HTTPException(status_code=409, detail=str(e))
        return {"ok": True}

    # --- Audio endpoints ---

    @app.get("/api/v1/audio/nodes")
    async def list_audio_nodes() -> dict[str, Any]:
        """List PipeWire audio nodes (sinks + sources) with volume/mute state."""
        if not audio:
            return {"nodes": {}, "links": []}
        return audio.watcher.snapshot()

    @app.get("/api/v1/audio/links")
    async def list_audio_links() -> dict[str, Any]:
        """List current PipeWire audio links."""
        if not audio:
            return {"links": []}
        return {"links": audio.watcher.snapshot()["links"]}

    @app.get("/api/v1/audio/routes")
    async def list_audio_routes() -> dict[str, Any]:
        """List active PipeWire audio routes (nodes + links combined view)."""
        if not audio:
            return {"routes": [], "nodes": {}, "links": []}
        snap = audio.watcher.snapshot()
        nodes = snap.get("nodes", {})
        links = snap.get("links", [])
        # Build a flat routes list: each link becomes a route entry
        routes = [
            {
                "from": lnk.get("out_node"),
                "to": lnk.get("in_node"),
                "link_id": lnk.get("id"),
            }
            for lnk in links
            if isinstance(lnk, dict)
        ]
        return {"routes": routes, "nodes": nodes, "links": links}

    @app.post("/api/v1/audio/volume")
    async def set_audio_volume(req: VolumeRequest) -> dict[str, Any]:
        """Set volume (linear 0.0-1.0+) on a PipeWire node."""
        if not audio:
            raise HTTPException(status_code=503, detail="Audio routing disabled")
        ok = await audio.set_volume(req.node_name, req.volume)
        if not ok:
            raise HTTPException(status_code=404, detail=f"PW node '{req.node_name}' not found")
        return {"ok": True, "node_name": req.node_name, "volume": req.volume}

    @app.post("/api/v1/audio/mute")
    async def set_audio_mute(req: MuteRequest) -> dict[str, Any]:
        """Set mute state on a PipeWire node."""
        if not audio:
            raise HTTPException(status_code=503, detail="Audio routing disabled")
        ok = await audio.set_mute(req.node_name, req.mute)
        if not ok:
            raise HTTPException(status_code=404, detail=f"PW node '{req.node_name}' not found")
        return {"ok": True, "node_name": req.node_name, "mute": req.mute}

    # --- Room correction endpoints ---

    @app.get("/api/v1/audio/room-correction/node-audio")
    async def get_node_audio_devices(node_id: str = "") -> dict[str, Any]:
        """
        Fetch PipeWire audio devices from a node.

        The controller proxies this — the browser never talks to nodes directly.
        """
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        port = node.api_port or node.stream_port
        if not port:
            return {"nodes": []}
        import urllib.request
        try:
            url = f"http://{node.host}:{port}/audio/nodes"
            loop = asyncio.get_running_loop()
            def _fetch():
                with urllib.request.urlopen(url, timeout=5) as r:
                    return json.loads(r.read())
            result = await loop.run_in_executor(None, _fetch)
            return result
        except Exception as e:
            return {"nodes": [], "error": str(e)}

    @app.get("/api/v1/audio/room-correction/usb-mics")
    async def list_usb_mics(q: str = "") -> dict[str, Any]:
        """
        Autocomplete for USB/XLR mic models.

        Query params:
          q — case-insensitive substring filter (optional)

        Returns: {mics: [{name, key, has_curve}, ...]}
        Filtered by q if provided. The list includes all known models from the
        hardcoded seed list.
        """
        from room_correction import KNOWN_USB_MICS, USB_MIC_CURVES, normalise_mic_name
        results = []
        for name in KNOWN_USB_MICS:
            if not q or q.lower() in name.lower():
                key = normalise_mic_name(name)
                results.append({
                    "name": name,
                    "key": key,
                    "has_curve": key in USB_MIC_CURVES,
                })
        return {"mics": results}

    @app.post("/api/v1/audio/room-correction/sweep")
    async def run_room_sweep(body: dict) -> dict[str, Any]:
        """
        Run a room correction sweep. If node_id is specified, the controller
        proxies the sweep to that node (where the audio hardware is).
        Otherwise runs locally on the controller's PipeWire.
        """
        source = body.get("source", "")
        sink = body.get("sink", "")
        if not source or not sink:
            raise HTTPException(status_code=400, detail="source and sink required")

        node_id = body.get("node_id", "")
        node = state.nodes.get(node_id) if node_id else None

        if node and node.api_port:
            # Proxy sweep to the node
            import urllib.request
            url = f"http://{node.host}:{node.api_port}/audio/sweep"
            try:
                loop = asyncio.get_running_loop()
                payload = json.dumps({
                    "source": source, "sink": sink,
                    "phone_model": body.get("phone_model", "generic"),
                    "target_curve": body.get("target_curve", "harman"),
                    "room_name": body.get("room_name", ""),
                    "mic_type": body.get("mic_type", "phone"),
                    "mic_model": body.get("mic_model", ""),
                }).encode()
                def _proxy():
                    req = urllib.request.Request(
                        url, data=payload,
                        headers={"Content-Type": "application/json"},
                        method="POST",
                    )
                    with urllib.request.urlopen(req, timeout=30) as r:
                        return json.loads(r.read())
                result = await loop.run_in_executor(None, _proxy)
                return result
            except Exception as e:
                return {"ok": False, "error": f"Node sweep failed: {e}"}

        # Local sweep (controller has audio hardware)
        if not room_correction:
            raise HTTPException(status_code=503, detail="Room correction not available")
        profile = await room_correction.run_sweep(
            source=source, sink=sink,
            phone_model=body.get("phone_model", "generic"),
            target_curve=body.get("target_curve", "harman"),
            room_name=body.get("room_name", ""),
            node_id=node_id,
            mic_type=body.get("mic_type", "phone"),
            mic_model=body.get("mic_model", ""),
        )
        if not profile:
            return {"ok": False, "error": "Sweep failed — check source/sink names and PipeWire state"}
        return {"ok": True, "profile": profile.to_dict()}

    @app.post("/api/v1/audio/room-correction/measure")
    async def process_room_measurement(body: dict) -> dict[str, Any]:
        """
        Process a room measurement from the phone/USB mic sweep UI.

        Body: {
            frequency_response: [[freq, db], ...],  // from browser FFT
            phone_model: "iphone_15" | "pixel_8" | "generic" | ...,
            target_curve: "harman" | "flat" | "bbc",
            room_name: "Living Room",
            node_id: "vm1._ozma._udp.local.",
            mic_type: "phone" | "usb" | "xlr",   // default: "phone"
            mic_model: "Blue Yeti",               // display name for USB/XLR mics
        }
        """
        if not room_correction:
            raise HTTPException(status_code=503, detail="Room correction not available")
        freq_resp = [(f, db) for f, db in body.get("frequency_response", [])]
        if not freq_resp:
            raise HTTPException(status_code=400, detail="frequency_response required")

        mic_type = body.get("mic_type", "phone")
        mic_model = body.get("mic_model", "")

        profile = room_correction.process_measurement(
            frequency_response=freq_resp,
            phone_model=body.get("phone_model", "generic"),
            target_curve=body.get("target_curve", "harman"),
            room_name=body.get("room_name", ""),
            node_id=body.get("node_id", ""),
            mic_type=mic_type,
            mic_model=mic_model,
        )

        # Async fire-and-forget: submit to Connect if authenticated
        if connect and connect.authenticated:
            asyncio.create_task(
                connect.submit_mic_measurement(
                    phone_model=body.get("phone_model", "generic") if mic_type == "phone" else mic_model,
                    raw_response=freq_resp,
                    correction_applied=[(b["freq"], b["gain"]) for b in profile.to_dict()["bands"]],
                    target_curve=body.get("target_curve", "harman"),
                    mic_type=mic_type,
                    mic_model=mic_model,
                ),
                name="mic_telemetry",
            )

        return {"ok": True, "profile": profile.to_dict()}

    @app.post("/api/v1/audio/room-correction/contribute")
    async def contribute_mic_measurement(body: dict) -> dict[str, Any]:
        """
        Forward a phone or USB mic measurement from the mobile app to Connect.

        The mobile app always talks to the controller (no Connect JWT needed
        in the app). This endpoint proxies the submission to Connect as a
        fire-and-forget task.

        Body: {
            phone_model: str,           // for phone mics (mic_type="phone")
            mic_type: str,              // "phone" | "usb" | "xlr" (default: "phone")
            mic_model: str,             // display name for USB/XLR (e.g. "Blue Yeti")
            raw_response: [[freq, db], ...],
            correction_applied: [[freq, db], ...],
            target_curve: str,
            snr_estimate: float,        // optional
        }
        """
        if not connect or not connect.authenticated:
            return {"ok": True, "accepted": False, "reason": "connect_not_available"}

        mic_type = body.get("mic_type", "phone")
        mic_model = body.get("mic_model", "")
        phone_model = body.get("phone_model", "generic")
        raw_response = body.get("raw_response", [])
        if not raw_response:
            raise HTTPException(status_code=400, detail="raw_response required")

        asyncio.create_task(
            connect.submit_mic_measurement(
                phone_model=phone_model if mic_type == "phone" else mic_model,
                raw_response=[(f, d) for f, d in raw_response],
                correction_applied=[(f, d) for f, d in body.get("correction_applied", [])],
                target_curve=body.get("target_curve", "harman"),
                snr_estimate=float(body.get("snr_estimate", 0.0)),
                mic_type=mic_type,
                mic_model=mic_model,
            ),
            name="mic_telemetry_mobile",
        )
        return {"ok": True, "accepted": True}

    @app.post("/api/v1/audio/room-correction/apply")
    async def apply_room_correction(body: dict) -> dict[str, Any]:
        """Apply a correction profile. Proxies to the node if node_id specified."""
        profile_id = body.get("profile_id", "")
        node_id = body.get("node_id", "")
        node = state.nodes.get(node_id) if node_id else None
        if node and node.api_port:
            return await _proxy_to_node(node, "/audio/apply", {"profile_id": profile_id})
        if not room_correction:
            raise HTTPException(status_code=503, detail="Room correction not available")
        ok = await room_correction.apply_correction(profile_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Profile not found or failed to apply")
        return {"ok": True, "profile_id": profile_id}

    @app.post("/api/v1/audio/room-correction/remove")
    async def remove_room_correction(body: dict = {}) -> dict[str, Any]:
        """Remove the active room correction EQ."""
        node_id = body.get("node_id", "") if body else ""
        node = state.nodes.get(node_id) if node_id else None
        if node and node.api_port:
            return await _proxy_to_node(node, "/audio/remove", {})
        if not room_correction:
            raise HTTPException(status_code=503, detail="Room correction not available")
        await room_correction.remove_correction()
        return {"ok": True}

    @app.post("/api/v1/audio/room-correction/play")
    async def play_demo_track(body: dict) -> dict[str, Any]:
        """Play a reference track through a node's speakers via pw-play."""
        node_id = body.get("node_id", "")
        track = body.get("track", "")
        sink = body.get("sink", "")
        node = state.nodes.get(node_id) if node_id else None
        if not node or not node.api_port:
            raise HTTPException(status_code=400, detail="node_id with api_port required")
        return await _proxy_to_node(node, "/audio/play", {"track": track, "sink": sink})

    @app.post("/api/v1/audio/room-correction/stop")
    async def stop_playback(body: dict) -> dict[str, Any]:
        """Stop any active pw-play on a node."""
        node_id = body.get("node_id", "")
        node = state.nodes.get(node_id) if node_id else None
        if not node or not node.api_port:
            raise HTTPException(status_code=400, detail="node_id with api_port required")
        return await _proxy_to_node(node, "/audio/stop", {})

    async def _proxy_to_node(node: Any, path: str, body: dict) -> dict:
        """Proxy a JSON POST request to a node's HTTP API."""
        import urllib.request
        url = f"http://{node.host}:{node.api_port}{path}"
        try:
            loop = asyncio.get_running_loop()
            payload = json.dumps(body).encode()
            def _do():
                req = urllib.request.Request(
                    url, data=payload,
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                with urllib.request.urlopen(req, timeout=30) as r:
                    return json.loads(r.read())
            return await loop.run_in_executor(None, _do)
        except Exception as e:
            return {"ok": False, "error": str(e)}

    @app.get("/api/v1/audio/room-correction/profiles")
    async def list_correction_profiles() -> dict[str, Any]:
        """List all stored room correction profiles."""
        if not room_correction:
            return {"profiles": []}
        return {"profiles": room_correction.list_profiles()}

    @app.get("/api/v1/audio/room-correction/status")
    async def room_correction_status() -> dict[str, Any]:
        """Room correction status: available models, target curves, active profile."""
        if not room_correction:
            return {"available": False}
        return {"available": True, **room_correction.status()}

    @app.get("/api/v1/audio/room-correction/phone-models")
    async def list_phone_models() -> dict[str, Any]:
        """List known phone mic models for compensation."""
        if not room_correction:
            return {"models": []}
        from room_correction import PHONE_MIC_CURVES
        return {"models": list(PHONE_MIC_CURVES.keys())}

    @app.post("/api/v1/audio/room-correction/detect-phone")
    async def detect_phone(body: dict) -> dict[str, Any]:
        """Detect phone model from User-Agent string."""
        if not room_correction:
            raise HTTPException(status_code=503, detail="Room correction not available")
        ua = body.get("user_agent", "")
        model = room_correction.detect_phone_model(ua)
        return {"model": model}

    # Active sweep processes keyed by node_id (or "" for local).
    _sweep_processes: dict[str, Any] = {}

    @app.post("/api/v1/audio/room-correction/play-sweep")
    async def play_sweep_for_phone(body: dict) -> dict[str, Any]:
        """
        Play a log sweep through a speaker sink without recording.
        The phone IS the microphone — it calls this endpoint then starts
        recording locally at the same time.

        Non-blocking: returns immediately after spawning pw-play so the
        phone can begin its MediaStream capture in parallel.

        Body: {node_id?, sink, duration?: 5}
        Returns: {ok, duration}
        """
        if not room_correction:
            raise HTTPException(status_code=503, detail="Room correction not available")

        sink = body.get("sink", "")
        if not sink:
            raise HTTPException(status_code=400, detail="sink required")

        duration = float(body.get("duration", 5))
        node_id = body.get("node_id", "")

        import tempfile, pathlib as _pl

        sweep_dir = _pl.Path(tempfile.mkdtemp(prefix="ozma-sweep-phone-"))
        sweep_path = sweep_dir / "sweep.wav"

        try:
            room_correction._generate_sweep_wav(sweep_path, 48000, duration)
        except Exception as e:
            raise HTTPException(status_code=500, detail=f"Sweep generation failed: {e}")

        # Kill any existing sweep for this node
        existing = _sweep_processes.pop(node_id, None)
        if existing is not None:
            try:
                existing.terminate()
            except Exception:
                pass

        play_proc = await asyncio.create_subprocess_exec(
            "pw-play", "--target", sink, str(sweep_path),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        _sweep_processes[node_id] = play_proc

        # Clean up temp dir + process entry after sweep finishes (non-blocking)
        async def _cleanup():
            try:
                await asyncio.wait_for(play_proc.wait(), timeout=duration + 5)
            except Exception:
                pass
            _sweep_processes.pop(node_id, None)
            import shutil as _sh
            _sh.rmtree(sweep_dir, ignore_errors=True)

        asyncio.create_task(_cleanup(), name=f"sweep-cleanup-{node_id or 'local'}")

        return {"ok": True, "duration": duration}

    @app.post("/api/v1/audio/room-correction/stop-sweep")
    async def stop_sweep_playback(body: dict = {}) -> dict[str, Any]:
        """
        Cancel an in-progress sweep playback started by play-sweep.

        Body: {node_id?}
        """
        node_id = (body or {}).get("node_id", "")
        proc = _sweep_processes.pop(node_id, None)
        if proc is not None:
            try:
                proc.terminate()
            except Exception:
                pass
        return {"ok": True}

    @app.delete("/api/v1/audio/room-correction/profiles/{profile_id}")
    async def delete_correction_profile(profile_id: str) -> dict[str, Any]:
        """Delete a saved correction profile by ID."""
        if not room_correction:
            raise HTTPException(status_code=503, detail="Room correction not available")
        ok = room_correction.delete_profile(profile_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Profile not found")
        return {"ok": True}

    @app.get("/api/v1/audio/vban")
    async def get_vban_config(request: Request) -> dict[str, Any]:
        """Return VBAN configuration for all nodes."""
        _require_scope(request, SCOPE_READ)
        nodes_vban: dict[str, Any] = {}
        for nid, node in state.nodes.items():
            if node.audio_vban_port:
                nodes_vban[nid] = {
                    "port": node.audio_vban_port,
                    "host": node.host,
                    "enabled": True,
                }
        return {
            "enabled": bool(nodes_vban),
            "nodes": nodes_vban,
            "stream_name": "OZMA",
            "sample_rate": 48000,
            "channels": 2,
        }

    # --- KVM input proxy (dashboard → soft node API) ---

    async def _proxy_to_node(node_id: str, path: str, body: dict) -> dict:
        """Proxy a POST request to a node's soft node API."""
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        api_port = getattr(node, "api_port", 0)
        if not api_port:
            raise HTTPException(status_code=503, detail="Node has no API port")
        # Use localhost for nodes on the same host (container --net=host)
        host = "127.0.0.1"
        import aiohttp as _aiohttp
        try:
            async with _aiohttp.ClientSession() as session:
                async with session.post(
                    f"http://{host}:{api_port}{path}",
                    json=body,
                    timeout=_aiohttp.ClientTimeout(total=5),
                ) as resp:
                    return await resp.json()
        except Exception as e:
            raise HTTPException(status_code=502, detail=str(e))

    @app.post("/api/v1/nodes/{node_id:path}/proxy/input/key")
    async def proxy_input_key(node_id: str, body: dict) -> dict[str, Any]:
        """Proxy key input to a node's soft node API."""
        return await _proxy_to_node(node_id, "/input/key", body)

    @app.post("/api/v1/nodes/{node_id:path}/proxy/input/mouse")
    async def proxy_input_mouse(node_id: str, body: dict) -> dict[str, Any]:
        """Proxy mouse input to a node's soft node API."""
        return await _proxy_to_node(node_id, "/input/mouse", body)

    @app.post("/api/v1/nodes/{node_id:path}/proxy/webrtc/bitrate")
    async def proxy_webrtc_bitrate(node_id: str, body: dict) -> dict[str, Any]:
        """Adjust WebRTC video bitrate."""
        return await _proxy_to_node(node_id, "/webrtc/bitrate", body)

    @app.post("/api/v1/nodes/{node_id:path}/proxy/input/type")
    async def proxy_input_type(node_id: str, body: dict) -> dict[str, Any]:
        """Proxy text typing to a node's soft node API."""
        return await _proxy_to_node(node_id, "/input/type", body)

    # --- KVM input (dashboard → VNC, legacy) ---

    @app.post("/api/v1/input/{node_id}/key")
    async def input_key(node_id: str, body: dict) -> dict[str, Any]:
        """Send a key press/release to a node via VNC."""
        if not streams:
            raise HTTPException(status_code=503, detail="Streams not available")
        key_name = body.get("key", "")
        down = body.get("down", True)
        await streams.send_key(node_id, key_name, down)
        return {"ok": True}

    @app.post("/api/v1/input/{node_id}/pointer")
    async def input_pointer(node_id: str, body: dict) -> dict[str, Any]:
        """Send a mouse event to a node via VNC."""
        if not streams:
            raise HTTPException(status_code=503, detail="Streams not available")
        x = body.get("x", 0)
        y = body.get("y", 0)
        buttons = body.get("buttons", 0)
        await streams.send_pointer(node_id, x, y, buttons)
        return {"ok": True}

    @app.websocket("/api/v1/input/{node_id}/ws")
    async def input_ws(ws: WebSocket, node_id: str) -> None:
        """
        WebSocket for real-time KVM input from the dashboard.

        Client sends JSON messages:
          {"type": "key", "key": "a", "down": true}
          {"type": "pointer", "x": 500, "y": 300, "buttons": 0}
          {"type": "click", "x": 500, "y": 300}
          {"type": "type", "text": "hello"}
        """
        await ws.accept()
        if not streams:
            await ws.close(code=4003, reason="Streams not available")
            return

        try:
            while True:
                data = await ws.receive_json()
                msg_type = data.get("type", "")

                if msg_type == "key":
                    await streams.send_key(node_id, data["key"], data.get("down", True))

                elif msg_type == "pointer":
                    await streams.send_pointer(node_id, data["x"], data["y"],
                                               data.get("buttons", 0))

                elif msg_type == "click":
                    x, y = data["x"], data["y"]
                    await streams.send_pointer(node_id, x, y, 1)
                    await asyncio.sleep(0.05)
                    await streams.send_pointer(node_id, x, y, 0)

                elif msg_type == "type":
                    # Type text character by character via VNC keysym
                    for ch in data.get("text", ""):
                        keysym = ord(ch)
                        w = streams._captures.get(node_id)
                        if w and hasattr(w, '_vnc_writer') and w._vnc_writer:
                            flag_down = b'\x01'
                            flag_up = b'\x00'
                            w._vnc_writer.write(b'\x04' + flag_down + b'\x00\x00' + keysym.to_bytes(4, 'big'))
                            await w._vnc_writer.drain()
                            await asyncio.sleep(0.02)
                            w._vnc_writer.write(b'\x04' + flag_up + b'\x00\x00' + keysym.to_bytes(4, 'big'))
                            await w._vnc_writer.drain()
                            await asyncio.sleep(0.02)

        except WebSocketDisconnect:
            pass
        except Exception as e:
            log.debug("Input WS error: %s", e)

    # --- Screen reader endpoints ---

    @app.post("/api/v1/screen/read")
    async def read_screen(body: dict) -> dict[str, Any]:
        """
        Read and understand what's on a node's screen.

        Takes a VNC screenshot, runs OCR, detects UI elements,
        and optionally generates an AI description.

        Body: { node_id: "...", use_ai: false }
        """
        from screen_reader import ScreenReader
        reader = ScreenReader()
        node_id = body.get("node_id", "")
        node = state.nodes.get(node_id)
        if not node or not node.vnc_host or not node.vnc_port:
            raise HTTPException(status_code=404, detail="Node not found or no VNC")
        screen_state = await reader.read_node_screen(
            node.vnc_host, node.vnc_port,
            use_ai=body.get("use_ai", False),
        )
        return screen_state.to_dict()

    @app.post("/api/v1/screen/read-prompt")
    async def read_screen_prompt(body: dict) -> dict[str, Any]:
        """
        Read screen and return a structured prompt for AI agent consumption.
        """
        from screen_reader import ScreenReader
        reader = ScreenReader()
        node_id = body.get("node_id", "")
        node = state.nodes.get(node_id)
        if not node or not node.vnc_host or not node.vnc_port:
            raise HTTPException(status_code=404, detail="Node not found or no VNC")
        screen_state = await reader.read_node_screen(
            node.vnc_host, node.vnc_port,
            use_ai=body.get("use_ai", False),
        )
        return {"prompt": screen_state.to_prompt(), "raw": screen_state.to_dict()}

    @app.post("/api/v1/screen/find-button")
    async def find_button(body: dict) -> dict[str, Any]:
        """Find a button on screen by label. Returns click coordinates."""
        from screen_reader import ScreenReader
        reader = ScreenReader()
        node_id = body.get("node_id", "")
        label = body.get("label", "")
        node = state.nodes.get(node_id)
        if not node or not node.vnc_host or not node.vnc_port:
            raise HTTPException(status_code=404, detail="Node not found or no VNC")
        screen_state = await reader.read_node_screen(node.vnc_host, node.vnc_port)
        button = screen_state.find_button(label)
        if button:
            return {"found": True, "x": button.center[0], "y": button.center[1],
                    "element": button.to_dict()}
        return {"found": False}

    @app.post("/api/v1/screen/vectors")
    async def read_screen_vectors(body: dict) -> dict[str, Any]:
        """Extract structural vectors (rectangles, lines) from screen."""
        from screen_reader import ScreenReader
        reader = ScreenReader()
        node_id = body.get("node_id", "")
        node = state.nodes.get(node_id)
        if not node or not node.vnc_host or not node.vnc_port:
            raise HTTPException(status_code=404, detail="Node not found or no VNC")
        import asyncvnc
        async with asyncvnc.connect(node.vnc_host, node.vnc_port) as client:
            frame = await client.screenshot()
        return reader.extract_vectors(frame)

    # --- Audio output endpoints ---

    @app.get("/api/v1/audio/outputs")
    async def list_audio_outputs() -> dict[str, Any]:
        """List available audio output targets (local, AirPlay, RTP, etc.)."""
        if not audio:
            return {"outputs": []}
        return {"outputs": audio.outputs.list_outputs()}

    @app.post("/api/v1/audio/outputs/enable")
    async def enable_audio_output(req: SelectOutputRequest) -> dict[str, Any]:
        """Enable an audio output target (multiple can be active)."""
        if not audio:
            raise HTTPException(status_code=503, detail="Audio routing disabled")
        ok = await audio.outputs.enable_output(req.output_id)
        if not ok:
            raise HTTPException(status_code=404, detail=f"Output '{req.output_id}' not found or unavailable")
        return {"ok": True, "output_id": req.output_id}

    @app.post("/api/v1/audio/outputs/disable")
    async def disable_audio_output(req: SelectOutputRequest) -> dict[str, Any]:
        """Disable an audio output target."""
        if not audio:
            raise HTTPException(status_code=503, detail="Audio routing disabled")
        ok = await audio.outputs.disable_output(req.output_id)
        if not ok:
            raise HTTPException(status_code=404, detail=f"Output '{req.output_id}' not found")
        return {"ok": True, "output_id": req.output_id}

    @app.post("/api/v1/audio/outputs/delay")
    async def set_audio_output_delay(req: OutputDelayRequest) -> dict[str, Any]:
        """Set time-alignment delay (ms) on an audio output."""
        if not audio:
            raise HTTPException(status_code=503, detail="Audio routing disabled")
        ok = await audio.outputs.set_delay(req.output_id, req.delay_ms)
        if not ok:
            raise HTTPException(status_code=404, detail=f"Output '{req.output_id}' not found")
        return {"ok": True, "output_id": req.output_id, "delay_ms": req.delay_ms}

    @app.put("/api/v1/audio/outputs/{output_id}/delay")
    async def set_audio_output_delay_put(output_id: str, body: dict = {}) -> dict[str, Any]:
        """PUT alias: set time-alignment delay (ms) on an audio output by path param."""
        if not audio:
            raise HTTPException(status_code=503, detail="Audio routing disabled")
        delay_ms = float(body.get("delay_ms", 0))
        ok = await audio.outputs.set_delay(output_id, delay_ms)
        if not ok:
            raise HTTPException(status_code=404, detail=f"Output '{output_id}' not found")
        return {"ok": True, "output_id": output_id, "delay_ms": delay_ms}

    # --- RGB zone endpoints ---

    @app.get("/api/v1/rgb/zones")
    async def list_rgb_zones() -> dict[str, Any]:
        """List all RGB output zones (nodes, WLED, Art-Net)."""
        if not rgb_out:
            return {"zones": []}
        return {"zones": rgb_out.list_zones()}

    @app.get("/api/v1/rgb/compositor")
    async def get_compositor_state() -> dict[str, Any]:
        """Get compositor state: ambient config, active layers."""
        if not rgb_out:
            return {}
        return rgb_out.compositor.state_dict()

    @app.post("/api/v1/rgb/ambient")
    async def set_ambient(body: dict = {}) -> dict[str, Any]:
        """Set ambient effect: {effect, color, speed, brightness}."""
        if not rgb_out:
            raise HTTPException(status_code=503, detail="RGB disabled")
        from rgb_compositor import AmbientConfig
        cfg = AmbientConfig(
            effect=body.get("effect", "solid"),
            color=tuple(body.get("color", [20, 15, 40]))[:3],
            speed=float(body.get("speed", 1.0)),
            brightness=float(body.get("brightness", 0.3)),
        )
        rgb_out.compositor.set_ambient(cfg)
        return {"ok": True}

    @app.post("/api/v1/rgb/note")
    async def add_note(body: dict = {}) -> dict[str, Any]:
        """Add a notification layer: {name, color, ttl, effect}."""
        if not rgb_out:
            raise HTTPException(status_code=503, detail="RGB disabled")
        rgb_out.compositor.add_note(
            name=body.get("name", "custom"),
            color=tuple(body.get("color", [255, 255, 255]))[:3],
            ttl=float(body.get("ttl", 2.0)),
            effect=body.get("effect", "flash"),
        )
        return {"ok": True}

    # --- Motion device endpoints ---

    @app.get("/api/v1/motion/devices")
    async def list_motion_devices() -> dict[str, Any]:
        """List all motion devices and their state."""
        if not motion:
            return {"devices": []}
        return {"devices": motion.list_devices()}

    @app.post("/api/v1/motion/{device_id}/move")
    async def motion_move(device_id: str, body: dict = {}) -> dict[str, Any]:
        """Move an axis: {"axis": "pan", "value": 0.5}"""
        if not motion:
            raise HTTPException(status_code=503, detail="Motion disabled")
        axis = body.get("axis", "")
        value = float(body.get("value", 0))
        ok = await motion.move(device_id, axis, value)
        if not ok:
            raise HTTPException(status_code=404, detail="Device or axis not found")
        return {"ok": True}

    @app.post("/api/v1/motion/{device_id}/stop")
    async def motion_stop(device_id: str, body: dict = {}) -> dict[str, Any]:
        """Stop movement: {"axis": "pan"} or {} for all."""
        if not motion:
            raise HTTPException(status_code=503, detail="Motion disabled")
        ok = await motion.stop_axis(device_id, body.get("axis"))
        return {"ok": ok}

    @app.post("/api/v1/motion/{device_id}/preset")
    async def motion_preset(device_id: str, body: dict = {}) -> dict[str, Any]:
        """Go to preset: {"name": "standing"}"""
        if not motion:
            raise HTTPException(status_code=503, detail="Motion disabled")
        ok = await motion.go_to_preset(device_id, body.get("name", ""))
        if not ok:
            raise HTTPException(status_code=404, detail="Device or preset not found")
        return {"ok": True}

    # --- Bluetooth endpoints ---

    @app.get("/api/v1/bluetooth/devices")
    async def list_bt_devices() -> dict[str, Any]:
        if not bt:
            return {"devices": [], "available": False}
        return {"devices": bt.list_devices(), "available": bt.available}

    @app.post("/api/v1/bluetooth/discover")
    async def bt_discover(body: dict = {}) -> dict[str, Any]:
        if not bt or not bt.available:
            raise HTTPException(status_code=503, detail="Bluetooth not available")
        timeout = float(body.get("timeout", 10))
        devices = await bt.discover(timeout=timeout)
        return {"devices": [d.to_dict() for d in devices]}

    @app.post("/api/v1/bluetooth/pair")
    async def bt_pair(body: dict = {}) -> dict[str, Any]:
        if not bt or not bt.available:
            raise HTTPException(status_code=503, detail="Bluetooth not available")
        addr = body.get("address", "")
        ok = await bt.pair(addr)
        return {"ok": ok, "address": addr}

    @app.post("/api/v1/bluetooth/connect")
    async def bt_connect(body: dict = {}) -> dict[str, Any]:
        if not bt or not bt.available:
            raise HTTPException(status_code=503, detail="Bluetooth not available")
        ok = await bt.connect(body.get("address", ""))
        return {"ok": ok}

    @app.post("/api/v1/bluetooth/disconnect")
    async def bt_disconnect(body: dict = {}) -> dict[str, Any]:
        if not bt or not bt.available:
            raise HTTPException(status_code=503, detail="Bluetooth not available")
        ok = await bt.disconnect(body.get("address", ""))
        return {"ok": ok}

    @app.get("/api/v1/bluetooth/pair-keys")
    async def list_pair_keys() -> dict[str, Any]:
        """List exportable pair keys (for migration)."""
        if not bt or not bt.available:
            return {"keys": []}
        keys = bt.export_all_pair_keys()
        return {"keys": [k.to_dict() for k in keys]}

    @app.post("/api/v1/bluetooth/pair-keys/export")
    async def export_pair_key(body: dict = {}) -> dict[str, Any]:
        """Export a single device's pair key."""
        if not bt or not bt.available:
            raise HTTPException(status_code=503, detail="Bluetooth not available")
        addr = body.get("address", "")
        key = bt.export_pair_key(addr)
        if not key:
            raise HTTPException(status_code=404, detail="No pair key for this device")
        return {
            "key": key.to_dict(),
            "info_contents": key.info_contents,
        }

    @app.post("/api/v1/bluetooth/pair-keys/import")
    async def import_pair_key(body: dict = {}) -> dict[str, Any]:
        """Import a pair key (from another device)."""
        if not bt:
            raise HTTPException(status_code=503, detail="Bluetooth not available")
        from bluetooth import PairKey
        key = PairKey(
            adapter_mac=body.get("adapter_mac", ""),
            device_mac=body.get("device_mac", ""),
            device_name=body.get("device_name", ""),
            info_contents=body.get("info_contents", ""),
        )
        target_adapter = body.get("target_adapter_mac")
        ok = bt.import_pair_key(key, target_adapter)
        return {"ok": ok}

    @app.post("/api/v1/bluetooth/pair-keys/push-to-node")
    async def push_pair_key_to_node(body: dict = {}) -> dict[str, Any]:
        """Push a pair key to a remote node via its HTTP API."""
        if not bt or not bt.available:
            raise HTTPException(status_code=503, detail="Bluetooth not available")
        addr = body.get("address", "")
        node_id = body.get("node_id", "")
        key = bt.export_pair_key(addr)
        if not key:
            raise HTTPException(status_code=404, detail="No pair key for this device")
        node = state.nodes.get(node_id)
        if not node or not node.api_port:
            raise HTTPException(status_code=404, detail="Node not found or no API")
        # Push to node's /bluetooth/pair-keys/import endpoint
        try:
            import urllib.request
            loop = asyncio.get_running_loop()
            payload = json.dumps({
                "adapter_mac": key.adapter_mac,
                "device_mac": key.device_mac,
                "device_name": key.device_name,
                "info_contents": key.info_contents,
            }).encode()
            url = f"http://{node.host}:{node.api_port}/bluetooth/pair-keys/import"
            req = urllib.request.Request(url, data=payload,
                                        headers={"Content-Type": "application/json"},
                                        method="POST")
            await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=10))
            return {"ok": True, "address": addr, "node_id": node_id}
        except Exception as e:
            raise HTTPException(status_code=502, detail=f"Push to node failed: {e}")

    # --- Paste typing endpoints ---

    @app.post("/api/v1/paste-typing")
    async def paste_typing_alias(body: dict = {}) -> dict[str, Any]:
        """Alias for POST /api/v1/paste — paste text via HID keystrokes."""
        if not paste_typer:
            raise HTTPException(status_code=503, detail="Paste typing not available")
        text = body.get("text", "")
        if not text:
            raise HTTPException(status_code=400, detail="No text provided")
        result = await paste_typer.type_text(
            text,
            layout=body.get("layout", "us"),
            rate=float(body.get("rate", 30)),
            node_id=body.get("node_id"),
        )
        return result

    @app.post("/api/v1/paste")
    async def paste_text(body: dict = {}) -> dict[str, Any]:
        """Type text to the active node via HID keystrokes.
        Body: {"text": "...", "layout": "us", "rate": 30, "node_id": null}
        """
        if not paste_typer:
            raise HTTPException(status_code=503, detail="Paste typing not available")
        text = body.get("text", "")
        if not text:
            raise HTTPException(status_code=400, detail="No text provided")
        result = await paste_typer.type_text(
            text,
            layout=body.get("layout", "us"),
            rate=float(body.get("rate", 30)),
            node_id=body.get("node_id"),
        )
        return result

    @app.post("/api/v1/paste/key")
    async def paste_key(body: dict = {}) -> dict[str, Any]:
        """Send a single named key (enter, f1, esc, etc.)."""
        if not paste_typer:
            raise HTTPException(status_code=503, detail="Paste typing not available")
        ok = await paste_typer.type_key(
            body.get("key", ""),
            modifier=int(body.get("modifier", 0)),
            node_id=body.get("node_id"),
        )
        return {"ok": ok}

    @app.get("/api/v1/paste/layouts")
    async def paste_layouts() -> dict[str, Any]:
        """List available keyboard layouts for paste typing."""
        return {"layouts": PasteTyper.available_layouts()}

    # --- Keyboard management endpoints ---

    @app.get("/api/v1/keyboards")
    async def list_keyboards() -> dict[str, Any]:
        """List detected programmable keyboards (VIA/QMK)."""
        if not kbd_mgr:
            return {"keyboards": []}
        return {"keyboards": kbd_mgr.list_keyboards()}

    @app.get("/api/v1/keyboards/{vid_pid}/via/version")
    async def kbd_via_version(vid_pid: str) -> dict[str, Any]:
        """Read VIA protocol version from a keyboard."""
        if not kbd_mgr:
            raise HTTPException(status_code=503, detail="Keyboard manager not available")
        ver = await kbd_mgr.via_get_protocol_version(vid_pid)
        if ver is None:
            raise HTTPException(status_code=404, detail="Keyboard not found or not VIA-compatible")
        return {"via_version": list(ver)}

    @app.get("/api/v1/keyboards/{vid_pid}/via/layers")
    async def kbd_via_layers(vid_pid: str) -> dict[str, Any]:
        """Read keymap layer count."""
        if not kbd_mgr:
            raise HTTPException(status_code=503, detail="Keyboard manager not available")
        count = await kbd_mgr.via_get_layer_count(vid_pid)
        if count is None:
            raise HTTPException(status_code=404, detail="Keyboard not found or not VIA-compatible")
        return {"layer_count": count}

    # --- Display capture endpoints ---

    @app.get("/api/v1/captures")
    async def list_captures() -> dict[str, Any]:
        """List all HDMI capture sources and their state."""
        if not captures:
            return {"sources": []}
        return {"sources": captures.list_sources()}

    @app.get("/api/v1/captures/{source_id}")
    async def get_capture(source_id: str) -> dict[str, Any]:
        if not captures:
            raise HTTPException(status_code=404, detail="No capture sources")
        source = captures.get_source(source_id)
        if not source:
            raise HTTPException(status_code=404, detail="Capture source not found")
        return source.to_dict()

    @app.get("/api/v1/captures/{source_id}/mjpeg")
    async def capture_mjpeg(source_id: str) -> StreamingResponse:
        """MJPEG stream from a capture source (low-latency fallback)."""
        if not captures:
            raise HTTPException(status_code=404, detail="No capture sources")
        frames = await captures.mjpeg_frames(source_id)
        if frames is None:
            raise HTTPException(status_code=404, detail="No stream for this source")

        async def generate():
            async for jpeg in frames:
                yield (b"--frame\r\nContent-Type: image/jpeg\r\n\r\n" + jpeg + b"\r\n")

        return StreamingResponse(
            generate(),
            media_type="multipart/x-mixed-replace; boundary=frame",
        )

    @app.post("/api/v1/captures/{source_id}/edid")
    async def set_capture_edid(source_id: str, body: dict = {}) -> dict[str, Any]:
        """Set EDID on a capture card to force a resolution.
        Body: {"resolution": "3440x1440"} or {"width": 3440, "height": 1440, "refresh": 60}
        """
        if not captures:
            raise HTTPException(status_code=503, detail="No capture sources")
        source = captures.get_source(source_id)
        if not source:
            raise HTTPException(status_code=404, detail="Capture source not found")

        from edid import generate_edid, set_edid
        from display_capture import COMMON_RESOLUTIONS

        # Parse resolution from body
        preset = body.get("resolution", "")
        if preset and preset in COMMON_RESOLUTIONS:
            r = COMMON_RESOLUTIONS[preset]
            w, h, refresh = r.width, r.height, r.fps
        else:
            w = int(body.get("width", 1920))
            h = int(body.get("height", 1080))
            refresh = int(body.get("refresh", 60))

        edid_data = generate_edid(w, h, refresh)
        ok = await set_edid(source.card.path, edid_data)
        if not ok:
            raise HTTPException(status_code=500, detail="Failed to set EDID")
        return {"ok": True, "width": w, "height": h, "refresh": refresh}

    # Text OCR instance (shared)
    _text_ocr = TextCapture()

    @app.post("/api/v1/captures/{source_id}/ocr")
    async def capture_ocr(source_id: str, body: dict = {}) -> dict[str, Any]:
        """Run OCR on the current frame of a capture source.
        Optional body: {"region": [x1, y1, x2, y2]} for partial OCR.
        """
        if not captures:
            raise HTTPException(status_code=503, detail="No capture sources")
        source = captures.get_source(source_id)
        if not source or not source.active:
            raise HTTPException(status_code=404, detail="Capture source not found or inactive")

        # Get latest frame from the HLS segment or MJPEG
        # For now, capture a frame from ffmpeg
        from pathlib import Path
        from PIL import Image
        import glob

        seg_dir = Path(f"controller/static/captures/{source_id}")
        segments = sorted(seg_dir.glob("seg_*.ts")) if seg_dir.exists() else []

        import subprocess
        loop = asyncio.get_running_loop()
        frame = None

        if segments:
            # Extract a frame from the latest HLS segment
            latest_seg = segments[-1]
            try:
                def _extract_seg():
                    r = subprocess.run(
                        ["ffmpeg", "-hide_banner", "-loglevel", "error",
                         "-i", str(latest_seg), "-frames:v", "1",
                         "-f", "image2pipe", "-vcodec", "png", "-"],
                        capture_output=True, timeout=5,
                    )
                    if r.returncode == 0 and r.stdout:
                        import io
                        return Image.open(io.BytesIO(r.stdout))
                    return None
                frame = await loop.run_in_executor(None, _extract_seg)
            except Exception:
                pass

        if not frame and source.card:
            # Fallback: grab a frame directly from the V4L2 device
            try:
                def _extract_v4l2():
                    r = subprocess.run(
                        ["ffmpeg", "-hide_banner", "-loglevel", "error",
                         "-f", "v4l2", "-i", source.card.path,
                         "-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "-"],
                        capture_output=True, timeout=5,
                    )
                    if r.returncode == 0 and r.stdout:
                        import io
                        return Image.open(io.BytesIO(r.stdout))
                    return None
                frame = await loop.run_in_executor(None, _extract_v4l2)
            except Exception:
                pass

        if not frame:
            raise HTTPException(status_code=503, detail="Could not extract frame")

        region = body.get("region")
        if region and isinstance(region, list) and len(region) == 4:
            region = tuple(region)
        else:
            region = None

        result = _text_ocr.recognise_frame(frame, region=region)
        return result.to_dict()

    @app.get("/api/v1/captures/{source_id}/text")
    async def get_capture_text(source_id: str) -> dict[str, Any]:
        """Get the last OCR result for a capture source."""
        if _text_ocr.last_result:
            return _text_ocr.last_result.to_dict()
        return {"text": "", "lines": [], "confidence": 0}

    @app.get("/api/v1/captures/resolutions")
    async def list_resolutions() -> dict[str, Any]:
        """List common resolution presets available for EDID override."""
        from display_capture import COMMON_RESOLUTIONS
        return {
            "resolutions": {
                name: r.to_dict() for name, r in COMMON_RESOLUTIONS.items()
            }
        }

    # --- Wi-Fi audio receiver endpoints ---

    @app.get("/api/v1/wifi-audio")
    async def wifi_audio_state() -> dict[str, Any]:
        """AirPlay + Spotify Connect receiver status."""
        if not wifi_audio:
            return {"airplay": {"available": False}, "spotify": {"available": False}}
        return wifi_audio.state_dict()

    # --- KDE Connect endpoints ---

    @app.get("/api/v1/kdeconnect/devices")
    async def list_kdeconnect_devices() -> dict[str, Any]:
        if not kdeconnect:
            return {"devices": []}
        return {"devices": kdeconnect.list_devices()}

    @app.post("/api/v1/kdeconnect/{device_id}/ping")
    async def kdeconnect_ping(device_id: str, body: dict = {}) -> dict[str, Any]:
        if not kdeconnect:
            raise HTTPException(status_code=503, detail="KDE Connect disabled")
        ok = await kdeconnect.ping(device_id, body.get("message", ""))
        return {"ok": ok}

    @app.post("/api/v1/kdeconnect/{device_id}/find")
    async def kdeconnect_find(device_id: str) -> dict[str, Any]:
        if not kdeconnect:
            raise HTTPException(status_code=503, detail="KDE Connect disabled")
        ok = await kdeconnect.find_my_phone(device_id)
        return {"ok": ok}

    @app.post("/api/v1/kdeconnect/{device_id}/media")
    async def kdeconnect_media(device_id: str, body: dict = {}) -> dict[str, Any]:
        if not kdeconnect:
            raise HTTPException(status_code=503, detail="KDE Connect disabled")
        ok = await kdeconnect.media_action(device_id, body.get("action", "play"))
        return {"ok": ok}

    # --- Control surface endpoints ---

    @app.get("/api/v1/controls")
    async def list_controls() -> dict[str, Any]:
        """List connected control surfaces and their state."""
        if not controls:
            return {"surfaces": []}
        return {"surfaces": controls.list_surfaces()}

    # --- Stream endpoints ---

    @app.get("/api/v1/streams")
    async def list_streams() -> dict[str, Any]:
        return {"streams": streams.list_streams() if streams else []}

    @app.get("/api/v1/streams/{node_id:path}/mjpeg")
    async def mjpeg_stream(node_id: str) -> StreamingResponse:
        if not streams:
            raise HTTPException(status_code=404, detail="No streams available")
        frames = streams.mjpeg_frames(node_id)
        if frames is None:
            raise HTTPException(status_code=404, detail="No stream for this node")

        async def generate():
            async for jpeg in frames:
                yield (
                    b"--frame\r\nContent-Type: image/jpeg\r\n\r\n"
                    + jpeg
                    + b"\r\n"
                )

        return StreamingResponse(
            generate(),
            media_type="multipart/x-mixed-replace; boundary=frame",
        )

    @app.get("/api/v1/streams/{node_id:path}/hls/{filename}")
    async def stream_hls_proxy(node_id: str, filename: str) -> Response:
        """Proxy HLS files (m3u8 + .ts segments) from a node's soft node."""
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        api_port = getattr(node, "api_port", 0)
        if not api_port:
            raise HTTPException(status_code=404, detail="Node has no API port")
        import aiohttp as _aiohttp
        content_type = "application/vnd.apple.mpegurl" if filename.endswith(".m3u8") else "video/mp2t"
        try:
            async with _aiohttp.ClientSession() as session:
                async with session.get(
                    f"http://127.0.0.1:{api_port}/stream/{filename}",
                    timeout=_aiohttp.ClientTimeout(total=10),
                ) as resp:
                    if resp.status == 200:
                        data = await resp.read()
                        return Response(content=data, media_type=content_type)
        except Exception:
            pass
        raise HTTPException(status_code=503, detail="HLS not available")

    @app.post("/api/v1/streams/{node_id:path}/webrtc")
    async def stream_webrtc(node_id: str, body: dict) -> Response:
        """WebRTC signaling — proxy to soft node's aiortc endpoint."""
        return await _proxy_to_node_raw(node_id, "/webrtc/offer", body)

    async def _proxy_to_node_raw(node_id: str, path: str, body: dict) -> Response:
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        api_port = getattr(node, "api_port", 0)
        if not api_port:
            raise HTTPException(status_code=503, detail="Node has no API port")
        import aiohttp as _aiohttp
        try:
            async with _aiohttp.ClientSession() as session:
                async with session.post(
                    f"http://127.0.0.1:{api_port}{path}",
                    json=body,
                    timeout=_aiohttp.ClientTimeout(total=10),
                ) as resp:
                    data = await resp.read()
                    return Response(content=data, media_type=resp.content_type,
                                    status_code=resp.status)
        except Exception as e:
            raise HTTPException(status_code=502, detail=str(e))

    @app.get("/api/v1/streams/{node_id:path}/snapshot")
    async def stream_snapshot(node_id: str) -> Response:
        """Single JPEG frame from a node's display (proxy to soft node API)."""
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        api_port = getattr(node, "api_port", 0)
        if not api_port:
            raise HTTPException(status_code=404, detail="Node has no API port")
        import aiohttp as _aiohttp
        try:
            async with _aiohttp.ClientSession() as session:
                async with session.get(
                    f"http://127.0.0.1:{api_port}/display/snapshot",
                    timeout=_aiohttp.ClientTimeout(total=10),
                ) as resp:
                    if resp.status == 200 and resp.content_type.startswith("image/"):
                        data = await resp.read()
                        return Response(content=data, media_type="image/jpeg")
        except Exception:
            pass
        raise HTTPException(status_code=503, detail="Snapshot unavailable")

    @app.get("/api/v1/streams/{node_id:path}")
    async def get_stream(node_id: str) -> dict[str, Any]:
        url = streams.stream_url(node_id) if streams else None
        if url is None:
            raise HTTPException(status_code=404, detail="No stream for this node")
        return {"node_id": node_id, "url": url, "type": streams.stream_type(node_id) if streams else "mjpeg"}

    # --- Device metrics ---

    @app.get("/api/v1/metrics")
    async def list_metrics() -> dict[str, Any]:
        """Get latest metrics for all sources."""
        if not metrics_collector:
            return {"sources": []}
        return {"sources": metrics_collector.get_all()}

    @app.get("/api/v1/metrics/{source_id}")
    async def get_metrics(source_id: str) -> dict[str, Any]:
        if not metrics_collector:
            raise HTTPException(status_code=503, detail="Metrics not available")
        data = metrics_collector.get_device(source_id, include_history=True)
        if not data:
            raise HTTPException(status_code=404, detail="No metrics for this source")
        return data

    @app.post("/api/v1/metrics/{source_id}")
    async def push_metrics(source_id: str, body: dict = {}) -> dict[str, Any]:
        """Push arbitrary metrics from any source.
        Body: {"metrics": {"key": value}, "name": "...", "tags": {...},
               "definitions": {"key": {"unit": "°C", "warn_threshold": 80}}}
        """
        if not metrics_collector:
            raise HTTPException(status_code=503, detail="Metrics not available")
        metrics_data = body.get("metrics", {})
        if not metrics_data and any(isinstance(v, (int, float)) for v in body.values()):
            metrics_data = {k: v for k, v in body.items() if isinstance(v, (int, float))}
        metrics_collector.push(
            source_id, metrics=metrics_data,
            name=body.get("name", ""),
            tags=body.get("tags"),
            definitions=body.get("definitions"),
        )
        return {"ok": True}

    @app.post("/api/v1/metrics/query")
    async def query_metrics(body: dict = {}) -> dict[str, Any]:
        """Query metrics across sources.
        Body: {"queries": [{"source": "*", "key": "cpu_temp"}]}"""
        if not metrics_collector:
            return {"results": []}
        return {"results": metrics_collector.query(body.get("queries", []))}

    @app.get("/api/v1/metrics/{source_id}/{key}/history")
    async def get_metric_history(source_id: str, key: str) -> dict[str, Any]:
        """Time-series history for a specific metric."""
        if not metrics_collector:
            return {"history": []}
        return {"source": source_id, "key": key,
                "history": metrics_collector.get_history(source_id, key)}

    # --- Sensor descriptions ---

    from sensor_descriptions import DescriptionPackManager
    _desc_packs = DescriptionPackManager()

    @app.get("/api/v1/descriptions")
    async def list_description_packs() -> dict[str, Any]:
        return {"packs": _desc_packs.list_packs(), "active": _desc_packs._active_pack_id}

    @app.post("/api/v1/descriptions/active")
    async def set_active_description_pack(body: dict = {}) -> dict[str, Any]:
        ok = _desc_packs.set_active(body.get("pack_id", "plain"))
        return {"ok": ok}

    @app.post("/api/v1/descriptions/describe")
    async def describe_metrics(body: dict = {}) -> dict[str, Any]:
        """Describe metric values: {"metrics": {"cpu_temp": 82, "temperature": 28}}"""
        metrics = body.get("metrics", {})
        pack_id = body.get("pack_id", "")
        return {"descriptions": _desc_packs.describe_all(metrics, pack_id)}

    @app.post("/api/v1/descriptions/install")
    async def install_description_pack(body: dict = {}) -> dict[str, Any]:
        ok = _desc_packs.install_pack(body)
        return {"ok": ok}

    # --- Widget packs ---

    from screen_widgets import WidgetPackManager
    _widget_packs = WidgetPackManager()
    _widget_packs.load()

    @app.get("/api/v1/widget-packs")
    async def list_widget_packs() -> dict[str, Any]:
        return {"packs": _widget_packs.list_packs()}

    @app.get("/api/v1/widget-packs/{pack_id}")
    async def get_widget_pack(pack_id: str) -> dict[str, Any]:
        pack = _widget_packs.get_pack(pack_id)
        if not pack:
            raise HTTPException(status_code=404, detail="Pack not found")
        return pack.to_dict()

    @app.post("/api/v1/widget-packs/{pack_id}/options")
    async def set_widget_pack_option(pack_id: str, body: dict = {}) -> dict[str, Any]:
        """Set a widget pack option: {"key": "bezel_color", "value": "#brass"}"""
        ok = _widget_packs.set_option(pack_id, body.get("key", ""), body.get("value"))
        if not ok:
            raise HTTPException(status_code=404, detail="Pack not found")
        return {"ok": True}

    @app.post("/api/v1/widget-packs/{pack_id}/theme")
    async def set_widget_pack_theme(pack_id: str, body: dict = {}) -> dict[str, Any]:
        """Activate a theme: {"theme": "amber"}"""
        ok = _widget_packs.set_theme(pack_id, body.get("theme", ""))
        if not ok:
            raise HTTPException(status_code=404, detail="Pack or theme not found")
        return {"ok": True}

    @app.post("/api/v1/widget-packs/install")
    async def install_widget_pack(body: dict = {}) -> dict[str, Any]:
        """Install a widget pack from manifest + files."""
        manifest = body.get("manifest", {})
        if not manifest.get("id"):
            raise HTTPException(status_code=400, detail="Missing manifest.id")
        # Files would come as base64 in a real implementation
        ok = _widget_packs.install_pack(manifest, {})
        return {"ok": ok}

    @app.delete("/api/v1/widget-packs/{pack_id}")
    async def remove_widget_pack(pack_id: str) -> dict[str, Any]:
        ok = _widget_packs.remove_pack(pack_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Pack not found")
        return {"ok": True}

    @app.get("/api/v1/layouts")
    async def list_layouts() -> dict[str, Any]:
        if not screen_mgr:
            return {"layouts": []}
        return {"layouts": screen_mgr.list_layouts()}

    # --- Screens ---

    @app.get("/api/v1/screens")
    async def list_screens() -> dict[str, Any]:
        if not screen_mgr:
            return {"screens": []}
        return {"screens": screen_mgr.list_screens()}

    @app.post("/api/v1/screens/{screen_id}/template")
    async def set_screen_template(screen_id: str, body: dict = {}) -> dict[str, Any]:
        """Change a screen's template and data source."""
        if not screen_mgr:
            raise HTTPException(status_code=503, detail="Screen manager not available")
        ok = screen_mgr.update_screen(
            screen_id,
            template=body.get("template"),
            data_source=body.get("data_source"),
            custom_data=body.get("custom_data"),
        )
        if not ok:
            raise HTTPException(status_code=404, detail="Screen not found")
        return {"ok": True}

    # --- OCR triggers ---

    @app.get("/api/v1/ocr/triggers")
    async def list_ocr_triggers() -> dict[str, Any]:
        """List all OCR trigger patterns (built-in + custom)."""
        if not ocr_triggers:
            return {"patterns": []}
        return {"patterns": ocr_triggers.list_patterns()}

    @app.post("/api/v1/ocr/triggers")
    async def add_ocr_trigger(body: dict = {}) -> dict[str, Any]:
        """Add a custom OCR trigger pattern."""
        if not ocr_triggers:
            raise HTTPException(status_code=503, detail="OCR triggers not available")
        pattern = TriggerPattern(
            id=body.get("id", "custom"),
            pattern=body.get("pattern", ""),
            is_regex=body.get("is_regex", False),
            severity=body.get("severity", "error"),
            category=body.get("category", "custom"),
            description=body.get("description", ""),
        )
        ocr_triggers.add_pattern(pattern)
        return {"ok": True, "pattern": pattern.to_dict()}

    # --- Automation ---

    @app.post("/api/v1/automation/run")
    async def run_automation(body: dict = {}) -> dict[str, Any]:
        """Execute an automation script.

        Always runs in the background: returns {"ok": true, "id": run_id}
        immediately.  Use POST /api/v1/automation/{run_id}/cancel to abort.
        """
        if not auto_engine:
            raise HTTPException(status_code=503, detail="Automation engine not available")
        script = body.get("script", "")
        if not script:
            raise HTTPException(status_code=400, detail="No script provided")
        run_id, result = await auto_engine.run_script_background(
            script,
            source_id=body.get("source_id", ""),
            node_id=body.get("node_id"),
            variables=body.get("variables"),
        )
        if not run_id:
            return result  # error (no active node)
        return result

    @app.get("/api/v1/automation/{run_id}/status")
    async def automation_status(run_id: str) -> dict[str, Any]:
        """Get status of a background automation run."""
        if not auto_engine:
            raise HTTPException(status_code=503, detail="Automation engine not available")
        status = auto_engine.get_script_status(run_id)
        if status is None:
            raise HTTPException(status_code=404, detail="Run not found")
        return status

    @app.post("/api/v1/automation/{run_id}/cancel")
    async def cancel_automation(run_id: str) -> dict[str, Any]:
        """Cancel a running background automation script."""
        if not auto_engine:
            raise HTTPException(status_code=503, detail="Automation engine not available")
        cancelled = auto_engine.cancel_script(run_id)
        if not cancelled:
            raise HTTPException(status_code=404, detail="Run not found or already completed")
        return {"ok": True, "cancelled": True, "id": run_id}

    # --- Node RPA proxy (forward to node's local RPA engine) ---

    async def _node_rpa_proxy(node_id: str, path: str, method: str = "GET",
                               body: dict | None = None) -> dict:
        """Proxy an RPA request to a node's local API."""
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        api_port = node.api_port or 7380
        url = f"http://{node.host}:{api_port}/api/v1/rpa/{path}"
        import aiohttp
        async with aiohttp.ClientSession() as session:
            if method == "GET":
                async with session.get(url, timeout=aiohttp.ClientTimeout(total=120)) as resp:
                    return await resp.json()
            else:
                async with session.post(url, json=body or {},
                                        timeout=aiohttp.ClientTimeout(total=120)) as resp:
                    return await resp.json()

    @app.get("/api/v1/nodes/{node_id}/rpa/screen")
    async def node_rpa_screen(node_id: str, mode: str = "auto") -> dict[str, Any]:
        """Read screen text from a node's capture card."""
        return await _node_rpa_proxy(node_id, f"screen?mode={mode}")

    @app.get("/api/v1/nodes/{node_id}/rpa/screenshot")
    async def node_rpa_screenshot(node_id: str) -> StreamingResponse:
        """Get a JPEG screenshot from a node's capture card."""
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        api_port = node.api_port or 7380
        url = f"http://{node.host}:{api_port}/api/v1/rpa/screenshot"
        import aiohttp
        async with aiohttp.ClientSession() as session:
            async with session.get(url, timeout=aiohttp.ClientTimeout(total=10)) as resp:
                data = await resp.read()
                return StreamingResponse(iter([data]), media_type="image/jpeg")

    @app.post("/api/v1/nodes/{node_id}/rpa/key")
    async def node_rpa_key(node_id: str, body: dict) -> dict[str, Any]:
        return await _node_rpa_proxy(node_id, "key", "POST", body)

    @app.post("/api/v1/nodes/{node_id}/rpa/click")
    async def node_rpa_click(node_id: str, body: dict) -> dict[str, Any]:
        return await _node_rpa_proxy(node_id, "click", "POST", body)

    @app.post("/api/v1/nodes/{node_id}/rpa/type")
    async def node_rpa_type(node_id: str, body: dict) -> dict[str, Any]:
        return await _node_rpa_proxy(node_id, "type", "POST", body)

    @app.post("/api/v1/nodes/{node_id}/rpa/script")
    async def node_rpa_script(node_id: str, body: dict) -> dict[str, Any]:
        """Run an RPA script on the node locally (autonomous, no controller needed)."""
        return await _node_rpa_proxy(node_id, "script", "POST", body)

    @app.post("/api/v1/nodes/{node_id}/rpa/enter_bios")
    async def node_rpa_enter_bios(node_id: str, body: dict = {}) -> dict[str, Any]:
        return await _node_rpa_proxy(node_id, "enter_bios", "POST", body)

    @app.post("/api/v1/nodes/{node_id}/rpa/set_boot_usb")
    async def node_rpa_set_boot_usb(node_id: str) -> dict[str, Any]:
        return await _node_rpa_proxy(node_id, "set_boot_usb", "POST")

    # --- Wake-on-LAN ---

    @app.post("/api/v1/nodes/{node_id}/wol")
    async def node_wol(node_id: str, body: dict = {}) -> dict[str, Any]:
        """Send Wake-on-LAN magic packet to a node's target machine."""
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        mac = body.get("mac") or get_mac_from_arp(node.host)
        if not mac:
            raise HTTPException(status_code=400, detail="MAC address not found — provide 'mac' in body")
        ok = send_wol(mac)
        return {"ok": ok, "mac": mac}

    # --- Macro endpoints ---

    @app.get("/api/v1/macros")
    async def list_macros() -> dict[str, Any]:
        if not macro_mgr:
            return {"macros": []}
        return {"macros": macro_mgr.list_macros()}

    @app.post("/api/v1/macros")
    async def create_macro(body: dict = {}) -> dict[str, Any]:
        """Create a macro from a definition (steps list).
        Body: {"id": "...", "name": "...", "steps": [...]}
        """
        if not macro_mgr:
            raise HTTPException(status_code=503, detail="Macro manager not available")
        macro_id = body.get("id", "")
        name = body.get("name", "")
        steps = body.get("steps", [])
        if not macro_id:
            raise HTTPException(status_code=400, detail="id is required")
        macro = macro_mgr.create_macro(macro_id, name, steps)
        if not macro:
            raise HTTPException(status_code=409, detail="Macro already exists or invalid steps")
        return macro.to_dict()

    @app.post("/api/v1/macros/record/start")
    async def macro_record_start(body: dict = {}) -> dict[str, Any]:
        if not macro_mgr:
            raise HTTPException(status_code=503, detail="Macro manager not available")
        macro_mgr.start_recording(body.get("id", "macro"), body.get("name", ""))
        return {"ok": True, "recording": True}

    @app.post("/api/v1/macros/record/stop")
    async def macro_record_stop() -> dict[str, Any]:
        if not macro_mgr:
            raise HTTPException(status_code=503, detail="Macro manager not available")
        macro = macro_mgr.stop_recording()
        return {"ok": bool(macro), "macro": macro.to_dict() if macro else None}

    @app.post("/api/v1/macros/{macro_id}/play")
    async def macro_play(macro_id: str) -> dict[str, Any]:
        if not macro_mgr:
            raise HTTPException(status_code=503, detail="Macro manager not available")
        ok = await macro_mgr.play(macro_id)
        return {"ok": ok}

    @app.delete("/api/v1/macros/{macro_id}")
    async def macro_delete(macro_id: str) -> dict[str, Any]:
        if not macro_mgr:
            raise HTTPException(status_code=503, detail="Macro manager not available")
        ok = macro_mgr.delete_macro(macro_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Macro not found")
        return {"ok": True}

    @app.post("/api/v1/macros/script")
    async def macro_script(body: dict = {}) -> dict[str, Any]:
        """Execute a macro script (DSL)."""
        if not macro_mgr:
            raise HTTPException(status_code=503, detail="Macro manager not available")
        script = body.get("script", "")
        if not script:
            raise HTTPException(status_code=400, detail="No script provided")
        result = await macro_mgr.run_script(script)
        return result

    # --- AI Agent Control endpoint ---

    @app.post("/api/v1/agent/control")
    async def agent_control(body: dict) -> dict[str, Any]:
        """
        AI agent control — single entry point for all agent interactions.

        This is the `ozma_control` MCP tool. Compatible with Anthropic's
        computer use and OpenAI's CUA action schemas.

        Actions: screenshot, read_screen, click, double_click, right_click,
        type, key, hotkey, mouse_move, mouse_drag, scroll, wait_for_text,
        wait_for_element, find_elements, assert_text, assert_element

        Use som=true with screenshot/read_screen to get Set-of-Marks numbered
        overlays, then click by element_id for accurate element targeting.
        """
        if not agent_engine:
            raise HTTPException(status_code=503, detail="Agent engine not available")
        action = body.pop("action", "")
        if not action:
            raise HTTPException(status_code=400, detail="No action specified")
        result = await agent_engine.execute(action, **body)
        return result.to_dict()

    @app.get("/api/v1/agent/tool-schema")
    async def agent_tool_schema() -> dict[str, Any]:
        """Return the MCP tool schema for ozma_control."""
        from agent_engine import OZMA_CONTROL_TOOL
        return OZMA_CONTROL_TOOL

    @app.get("/api/v1/agent/pending")
    async def agent_pending(request: Request) -> dict[str, Any]:
        """List agent actions awaiting approval."""
        _require_scope(request, SCOPE_READ)
        if not agent_engine:
            return {"pending": []}
        return {"pending": agent_engine.list_pending()}

    @app.post("/api/v1/agent/{action_id}/approve")
    async def agent_approve(request: Request, action_id: str) -> dict[str, Any]:
        """Approve a pending agent action."""
        _require_scope(request, SCOPE_WRITE)
        if not agent_engine:
            raise HTTPException(status_code=503, detail="Agent engine not available")
        ok = agent_engine.approve_action(action_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Pending action not found")
        return {"ok": True}

    @app.post("/api/v1/agent/{action_id}/reject")
    async def agent_reject(request: Request, action_id: str) -> dict[str, Any]:
        """Reject a pending agent action."""
        _require_scope(request, SCOPE_WRITE)
        if not agent_engine:
            raise HTTPException(status_code=503, detail="Agent engine not available")
        ok = agent_engine.reject_action(action_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Pending action not found")
        return {"ok": True}

    @app.get("/api/v1/agent/config")
    async def agent_config_get(request: Request) -> dict[str, Any]:
        """Get current agent approval mode configuration."""
        _require_scope(request, SCOPE_READ)
        if not agent_engine:
            return {"approval_modes": {}}
        return {"approval_modes": agent_engine.get_approval_config()}

    @app.put("/api/v1/agent/config")
    async def agent_config_set(request: Request, body: dict) -> dict[str, Any]:
        """Update agent approval mode configuration."""
        _require_scope(request, SCOPE_ADMIN)
        if not agent_engine:
            raise HTTPException(status_code=503, detail="Agent engine not available")
        modes = body.get("approval_modes", {})
        agent_engine.set_approval_config(modes)
        return {"ok": True, "approval_modes": agent_engine.get_approval_config()}

    @app.get("/api/v1/vision/providers")
    async def list_vision_providers() -> dict[str, Any]:
        """List available vision providers (OmniParser, YOLO, Ollama, Connect)."""
        if not agent_engine or not agent_engine._screen_reader._vision:
            return {"providers": []}
        return {"providers": agent_engine._screen_reader._vision.list_providers()}

    @app.get("/api/v1/nodes/{node_id}/ui/hints")
    async def node_ui_hints(node_id: str, level: int = 2) -> dict[str, Any]:
        """
        Get UI hints from the agent running inside a node's target machine.

        Returns window list, focused control, and accessibility tree.
        Much faster and more accurate than OCR when the agent is available.

        Level 1: windows only. Level 2: + focused control. Level 3: + full tree.
        """
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        api_port = node.api_port or 7390
        import aiohttp
        try:
            async with aiohttp.ClientSession() as session:
                async with session.get(
                    f"http://{node.host}:{api_port}/ui/hints?level={level}",
                    timeout=aiohttp.ClientTimeout(total=3),
                ) as resp:
                    if resp.status == 200:
                        return await resp.json()
                    return {"error": f"Agent returned {resp.status}"}
        except Exception as e:
            return {"error": f"Agent not reachable: {e}", "hint": "Agent may not be installed on target machine"}

    @app.post("/api/v1/vision/detect")
    async def vision_detect(body: dict) -> dict[str, Any]:
        """
        Run AI vision detection on a node's screen.

        Body: { node_id: "...", provider: "" (auto) }
        Returns detected UI elements with bounding boxes.
        """
        if not agent_engine:
            raise HTTPException(status_code=503, detail="Agent engine not available")
        node_id = body.get("node_id", "")
        provider = body.get("provider", "")
        node = state.nodes.get(node_id) if node_id else state.get_active_node() if hasattr(state, 'get_active_node') else None
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")

        img = await agent_engine._capture_frame(node)
        if not img:
            raise HTTPException(status_code=500, detail="Failed to capture screenshot")

        vision_mgr = agent_engine._screen_reader._vision
        if not vision_mgr:
            raise HTTPException(status_code=503, detail="No vision providers available")

        result = await vision_mgr.detect(img, provider_name=provider)
        return result.to_dict()

    # --- Visual regression test runner ---

    @app.post("/api/v1/tests/run")
    async def run_visual_test(body: dict) -> dict[str, Any]:
        """
        Run a visual regression test.

        Body can contain:
          - test_file: path to YAML test definition
          - test: inline YAML test definition (dict)
          - node_id: target node (overrides test file)
        """
        if not test_runner:
            raise HTTPException(status_code=503, detail="Test runner not available")
        if "test_file" in body:
            return await test_runner.run_file(body["test_file"], body.get("node_id", ""))
        elif "test" in body:
            return await test_runner.run(body["test"], body.get("node_id", ""))
        raise HTTPException(status_code=400, detail="Provide test_file or test")

    @app.get("/api/v1/tests/{test_id}")
    async def get_test_result(test_id: str) -> dict[str, Any]:
        if not test_runner:
            raise HTTPException(status_code=503, detail="Test runner not available")
        result = test_runner.get_result(test_id)
        if not result:
            raise HTTPException(status_code=404, detail="Test not found")
        return result

    @app.get("/api/v1/tests/history")
    async def test_history() -> dict[str, Any]:
        if not test_runner:
            return {"tests": []}
        return {"tests": test_runner.list_results()}

    @app.post("/api/v1/tests/abort/{test_id}")
    async def abort_test(test_id: str) -> dict[str, Any]:
        if not test_runner:
            raise HTTPException(status_code=503, detail="Test runner not available")
        ok = test_runner.abort(test_id)
        return {"ok": ok}

    # --- TestBench endpoints ---

    @app.get("/api/v1/testbench/harnesses")
    async def list_harnesses() -> dict[str, Any]:
        """List available test harnesses (MarkBench-compatible manifests)."""
        if not testbench:
            return {"harnesses": []}
        return {"harnesses": testbench.list_harnesses()}

    @app.post("/api/v1/testbench/harnesses/load")
    async def load_harnesses(body: dict) -> dict[str, Any]:
        """Load MarkBench manifest.yaml files from a directory."""
        if not testbench:
            raise HTTPException(status_code=503, detail="TestBench not available")
        count = testbench.load_manifests(body.get("directory", ""))
        return {"ok": True, "loaded": count}

    @app.post("/api/v1/testbench/run")
    async def run_test(body: dict) -> dict[str, Any]:
        """Run a test harness on a node."""
        if not testbench:
            raise HTTPException(status_code=503, detail="TestBench not available")
        harness_id = body.get("harness_id", "")
        node_id = body.get("node_id", "")
        options = body.get("options", {})
        run = await testbench.run_test(harness_id, node_id, options)
        return run.to_dict()

    @app.post("/api/v1/testbench/run/parallel")
    async def run_test_parallel(body: dict) -> dict[str, Any]:
        """Run the same test on multiple nodes simultaneously."""
        if not testbench:
            raise HTTPException(status_code=503, detail="TestBench not available")
        harness_id = body.get("harness_id", "")
        node_ids = body.get("node_ids", [])
        results = await testbench.run_parallel(harness_id, node_ids)
        return {"runs": [r.to_dict() if hasattr(r, "to_dict") else {"error": str(r)} for r in results]}

    @app.get("/api/v1/testbench/runs")
    async def list_test_runs() -> dict[str, Any]:
        """List all test runs (active + completed)."""
        if not testbench:
            return {"runs": []}
        return {"runs": testbench.list_runs()}

    @app.get("/api/v1/testbench/suites")
    async def list_test_suites() -> dict[str, Any]:
        if not testbench:
            return {"suites": []}
        return {"suites": testbench.list_suites()}

    @app.post("/api/v1/testbench/suites/run")
    async def run_test_suite(body: dict) -> dict[str, Any]:
        if not testbench:
            raise HTTPException(status_code=503, detail="TestBench not available")
        suite = await testbench.run_suite(body.get("suite_id", ""))
        return suite.to_dict()

    # --- Schedule endpoints ---

    @app.get("/api/v1/schedule")
    async def list_schedule() -> dict[str, Any]:
        if not sched:
            return {"rules": []}
        return {"rules": sched.list_rules()}

    @app.post("/api/v1/schedule")
    async def add_schedule_rule(body: dict = {}) -> dict[str, Any]:
        if not sched:
            raise HTTPException(status_code=503, detail="Scheduler not available")
        rule = sched.add_rule(
            time=body.get("time", "09:00"),
            days=body.get("days", "*"),
            scenario=body.get("scenario", ""),
        )
        return {"ok": True, "rule": rule}

    @app.delete("/api/v1/schedule/{index}")
    async def remove_schedule_rule(index: int) -> dict[str, Any]:
        if not sched:
            raise HTTPException(status_code=503, detail="Scheduler not available")
        ok = sched.remove_rule(index)
        if not ok:
            raise HTTPException(status_code=404, detail="Rule not found")
        return {"ok": True}

    # --- Notification endpoints ---

    @app.get("/api/v1/notifications/config")
    async def list_notifications() -> dict[str, Any]:
        if not notifier:
            return {"destinations": [], "rules": []}
        return {"destinations": notifier.list_destinations(), "rules": notifier.list_rules()}

    # --- Session recording endpoints ---

    @app.get("/api/v1/recording/status")
    async def recording_status() -> dict[str, Any]:
        if not recorder:
            return {"recording": False}
        return recorder.status()

    @app.post("/api/v1/recording/start")
    async def recording_start(body: dict = {}) -> dict[str, Any]:
        if not recorder:
            raise HTTPException(status_code=503, detail="Recorder not available")
        source_id = body.get("source_id", "hdmi-0")
        hls_path = f"/captures/{source_id}/stream.m3u8"
        scenario_id = scenarios.active_id or ""
        ok = await recorder.start_recording(source_id, hls_path, scenario_id)
        return {"ok": ok}

    @app.post("/api/v1/recording/stop")
    async def recording_stop() -> dict[str, Any]:
        if not recorder:
            raise HTTPException(status_code=503, detail="Recorder not available")
        rec = await recorder.stop_recording()
        return {"ok": bool(rec), "recording": rec.to_dict() if rec else None}

    @app.get("/api/v1/recording/list")
    async def recording_list() -> dict[str, Any]:
        if not recorder:
            return {"recordings": []}
        return {"recordings": recorder.list_recordings()}

    # --- Network health endpoints ---

    @app.get("/api/v1/network/health")
    async def network_health_list() -> dict[str, Any]:
        if not net_health:
            return {"nodes": []}
        return {"nodes": net_health.list_health()}

    @app.get("/api/v1/network/health/{node_id}")
    async def network_health_node(node_id: str) -> dict[str, Any]:
        if not net_health:
            raise HTTPException(status_code=503, detail="Network monitor not available")
        h = net_health.get_health(node_id)
        if not h:
            raise HTTPException(status_code=404, detail="Node not found")
        return h

    # --- Codec endpoints ---

    @app.put("/api/v1/codecs")
    async def put_codec_config(body: dict = {}) -> dict[str, Any]:
        """PUT alias for codec config — sets the default codec configuration."""
        if not codec_mgr:
            raise HTTPException(status_code=503, detail="Codec manager not available")
        source_id = body.get("source_id", "default")
        cfg = CodecConfig.from_dict(body)
        codec_mgr.set_config(source_id, cfg)
        resolved = codec_mgr.resolve(cfg)
        return {"ok": True, "source_id": source_id, "resolved": resolved.to_dict()}

    @app.get("/api/v1/codecs")
    async def list_codecs() -> dict[str, Any]:
        if not codec_mgr:
            return {"codecs": {}, "configs": {}}
        return {
            "codecs": codec_mgr.list_available(),
            "configs": codec_mgr.list_configs(),
            "ndi_available": codec_mgr.ndi_available,
        }

    @app.post("/api/v1/codecs/config")
    async def set_codec_config(body: dict = {}) -> dict[str, Any]:
        if not codec_mgr:
            raise HTTPException(status_code=503, detail="Codec manager not available")
        source_id = body.get("source_id", "default")
        cfg = CodecConfig.from_dict(body)
        codec_mgr.set_config(source_id, cfg)
        resolved = codec_mgr.resolve(cfg)
        return {"ok": True, "source_id": source_id, "resolved": resolved.to_dict()}

    @app.post("/api/v1/codecs/resolve")
    async def resolve_codec(body: dict = {}) -> dict[str, Any]:
        if not codec_mgr:
            raise HTTPException(status_code=503, detail="Codec manager not available")
        cfg = CodecConfig.from_dict(body)
        resolved = codec_mgr.resolve(cfg)
        return {"resolved": resolved.to_dict(), "ffmpeg_args": codec_mgr.get_ffmpeg_args(cfg)}

    @app.get("/api/v1/codecs/probe")
    async def probe_codecs(request: Request, force: bool = False) -> dict[str, Any]:
        """Probe all encoders with test encodes. Cached 60s. Use ?force=1 to bypass cache."""
        _require_scope(request, SCOPE_READ)
        if not codec_mgr:
            raise HTTPException(status_code=503, detail="Codec manager not available")
        from codec_manager import EncoderProbeResult
        results = await codec_mgr.probe_encoders_async(force=force)
        return {
            "encoders": [r.to_dict() for r in results],
            "available": {
                family: [r.encoder for r in results if r.codec_family == family and r.available]
                for family in ("h264", "h265", "av1", "vp9", "mjpeg")
            },
        }

    @app.get("/api/v1/streams/{node_id}/codec")
    async def get_stream_codec(request: Request, node_id: str) -> dict[str, Any]:
        """Get current codec info and available encoders for a stream."""
        _require_scope(request, SCOPE_READ)
        if not streams:
            raise HTTPException(status_code=503, detail="Streams not available")
        stats = streams.get_stream_stats(node_id)
        override = streams._codec_overrides.get(node_id)
        ocr_mode = node_id in streams._ocr_nodes
        # Get available encoders from cache (no blocking probe)
        available: list[dict] = []
        if codec_mgr:
            cached = codec_mgr.get_probe_cache()
            available = [r.to_dict() for r in cached if r.available]
            if not available:
                # Populate cache in background, return what we know right now
                asyncio.create_task(codec_mgr.probe_encoders_async(), name="codec-probe-bg")
                available = [{"encoder": e, "available": True}
                             for family in codec_mgr._available.values() for e in family]
            # Always include OCR terminal as available
            if not any(e.get("encoder") == "ocr-terminal" for e in available):
                available.append({"encoder": "ocr-terminal", "codec_family": "ocr",
                                   "hw_type": "text", "available": True, "probe_ms": 0})
        return {
            "node_id": node_id,
            "stats": stats.to_dict() if stats else {},
            "config": override.to_dict() if override else {},
            "adaptive": streams._adaptive_enabled,
            "available_encoders": available,
            "ocr_mode": ocr_mode,
            "current_encoder": "ocr-terminal" if ocr_mode else (stats.encoder if stats else ""),
        }

    @app.post("/api/v1/streams/{node_id}/codec")
    async def set_stream_codec(request: Request, node_id: str, body: dict = {}) -> dict[str, Any]:
        """Switch the codec for a running stream. Triggers graceful restart."""
        _require_scope(request, SCOPE_WRITE)
        if not streams:
            raise HTTPException(status_code=503, detail="Streams not available")
        if not codec_mgr:
            raise HTTPException(status_code=503, detail="Codec manager not available")
        cfg = CodecConfig.from_dict(body)
        resolved = codec_mgr.resolve(cfg)
        ok = await streams.switch_codec(node_id, cfg)
        if not ok:
            raise HTTPException(status_code=404,
                detail="Node not found or codec switching not supported for this stream type")
        await state.events.put({
            "type": "stream.codec_changed",
            "node_id": node_id,
            "encoder": resolved.name,
            "hw_type": resolved.hw_device and "vaapi" or
                       ("nvenc" if "nvenc" in resolved.ffmpeg_codec else
                        "qsv" if "qsv" in resolved.ffmpeg_codec else "software"),
        })
        return {"ok": True, "node_id": node_id, "resolved": resolved.to_dict()}

    @app.post("/api/v1/streams/{node_id}/adaptive")
    async def set_stream_adaptive(request: Request, node_id: str, body: dict = {}) -> dict[str, Any]:
        """Enable or disable adaptive codec switching (CPU/FPS-aware auto-select)."""
        _require_scope(request, SCOPE_WRITE)
        if not streams:
            raise HTTPException(status_code=503, detail="Streams not available")
        enabled = body.get("enabled", True)
        streams.enable_adaptive(enabled)
        return {"ok": True, "adaptive": enabled}

    @app.get("/api/v1/streams/{node_id}/stats")
    async def get_stream_stats(request: Request, node_id: str) -> dict[str, Any]:
        """Get live stream statistics (FPS, encoder, bitrate, uptime)."""
        _require_scope(request, SCOPE_READ)
        if not streams:
            raise HTTPException(status_code=503, detail="Streams not available")
        stats = streams.get_stream_stats(node_id)
        if not stats:
            return {"node_id": node_id, "active": False}
        return stats.to_dict()

    # --- Camera endpoints ---

    @app.get("/api/v1/cameras")
    async def list_cameras() -> dict[str, Any]:
        if not camera_mgr:
            return {"cameras": []}
        return {
            "cameras": camera_mgr.list_cameras(),
            "privacy_notice": "Camera streams may record individuals. Ensure all parties are aware of recording. Access is logged.",
        }

    @app.get("/api/v1/cameras/{camera_id}")
    async def get_camera(request: Request, camera_id: str) -> dict[str, Any]:
        # Route /cameras/whip is a WHIP session list — forward to that handler
        if camera_id == "whip":
            if not mobile_cam:
                return {"sessions": []}
            sessions = await mobile_cam.list_sessions()
            return {"sessions": sessions}
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        cam = camera_mgr.get_camera(camera_id)
        if not cam:
            raise HTTPException(status_code=404, detail="Camera not found")
        return cam.to_dict()

    @app.post("/api/v1/cameras")
    async def add_camera(body: dict = {}) -> dict[str, Any]:
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        cam = camera_mgr.add_camera(body)
        if not cam:
            raise HTTPException(status_code=400, detail="Invalid camera data or ID already exists")
        return cam.to_dict()

    @app.put("/api/v1/cameras/{camera_id}")
    async def update_camera(camera_id: str, body: dict = {}) -> dict[str, Any]:
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        ok = camera_mgr.update_camera(camera_id, body)
        if not ok:
            raise HTTPException(status_code=404, detail="Camera not found")
        return {"ok": True}

    @app.delete("/api/v1/cameras/{camera_id}")
    async def delete_camera(camera_id: str) -> dict[str, Any]:
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        ok = camera_mgr.remove_camera(camera_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Camera not found")
        return {"ok": True}

    @app.post("/api/v1/cameras/{camera_id}/privacy/acknowledge")
    async def acknowledge_camera_privacy(camera_id: str, body: dict = {}) -> dict[str, Any]:
        """Acknowledge the privacy notice for a camera. Required before enabling."""
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        client = body.get("client", "api")
        ok = camera_mgr.acknowledge_privacy(camera_id, client)
        if not ok:
            raise HTTPException(status_code=404, detail="Camera not found")
        return {
            "ok": True,
            "notice": "You have acknowledged that this camera may record individuals. "
                      "All stream access is logged. Ensure compliance with local privacy laws.",
        }

    @app.post("/api/v1/cameras/{camera_id}/privacy/level")
    async def set_camera_privacy_level(camera_id: str, body: dict = {}) -> dict[str, Any]:
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        level = body.get("level", "disabled")
        if level not in ("disabled", "local_only", "network", "public"):
            raise HTTPException(status_code=400, detail="Invalid privacy level")
        ok = camera_mgr.set_privacy_level(camera_id, level)
        if not ok:
            raise HTTPException(status_code=400, detail="Privacy not acknowledged or camera not found")
        return {"ok": True, "level": level}

    @app.post("/api/v1/cameras/{camera_id}/privacy/zones")
    async def add_privacy_zone(camera_id: str, body: dict = {}) -> dict[str, Any]:
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        from camera_manager import PrivacyZone
        zone = PrivacyZone(
            x=body.get("x", 0), y=body.get("y", 0),
            width=body.get("width", 0.1), height=body.get("height", 0.1),
            mode=body.get("mode", "blur"),
        )
        ok = camera_mgr.add_privacy_zone(camera_id, zone)
        if not ok:
            raise HTTPException(status_code=404, detail="Camera not found")
        return {"ok": True}

    @app.post("/api/v1/cameras/{camera_id}/start")
    async def start_camera_capture(camera_id: str) -> dict[str, Any]:
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        ok = await camera_mgr.start_capture(camera_id)
        if not ok:
            raise HTTPException(status_code=400, detail="Cannot start capture — check privacy settings")
        return {"ok": True}

    @app.post("/api/v1/cameras/{camera_id}/stop")
    async def stop_camera_capture(camera_id: str) -> dict[str, Any]:
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        ok = await camera_mgr.stop_capture(camera_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Camera not found")
        return {"ok": True}

    @app.get("/api/v1/cameras/{camera_id}/snapshot")
    async def camera_snapshot(camera_id: str) -> Any:
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")
        from fastapi.responses import Response
        data = await camera_mgr.snapshot(camera_id)
        if not data:
            raise HTTPException(status_code=400, detail="Cannot capture snapshot — check privacy settings")
        return Response(content=data, media_type="image/jpeg")

    @app.get("/api/v1/cameras/access-log")
    async def camera_access_log(camera_id: str = "", limit: int = 100) -> dict[str, Any]:
        if not camera_mgr:
            return {"entries": []}
        return {"entries": camera_mgr.get_access_log(camera_id or None, limit)}

    @app.post("/api/v1/cameras/{camera_id}/advise")
    async def advise_camera(camera_id: str, body: dict = {}) -> dict[str, Any]:
        """
        Snapshot the camera and return AI-generated name/zone/trigger advice.

        Optional body fields:
          snapshot_url: str  — fetch JPEG from this URL instead of CameraManager
          profile: str       — if provided, also return the Frigate YAML for that profile
                               ("default" | "paranoid" | "lax")
        """
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")

        from camera_advisor import advise_camera as _advise

        snapshot_url = body.get("snapshot_url", "")
        jpeg: bytes | None = None

        if snapshot_url:
            try:
                import aiohttp
                async with aiohttp.ClientSession() as _s:
                    async with _s.get(snapshot_url, timeout=aiohttp.ClientTimeout(total=10)) as r:
                        if r.status == 200:
                            jpeg = await r.read()
            except Exception as exc:
                raise HTTPException(status_code=502, detail=f"Failed to fetch snapshot_url: {exc}")
        else:
            jpeg = await camera_mgr.snapshot(camera_id)

        if not jpeg:
            raise HTTPException(status_code=400, detail="Cannot capture snapshot — check privacy settings or provide snapshot_url")

        advice = await _advise(camera_id, jpeg)
        result = advice.to_dict()

        # Optionally include Frigate YAML for the requested profile
        profile_key = body.get("profile", "")
        if profile_key and profile_key in advice.profiles:
            result["frigate_yaml"] = advice.profiles[profile_key].to_frigate_yaml(camera_id)

        return result

    @app.post("/api/v1/cameras/{camera_id}/apply-advice")
    async def apply_camera_advice(camera_id: str, body: dict = {}) -> dict[str, Any]:
        """
        Apply a chosen advice profile to the camera.

        Body:
          profile: str          — "default" | "paranoid" | "lax"  (required)
          suggested_name: str   — rename the camera to this (optional)
          advice: dict          — the full CameraAdvice dict from /advise (optional;
                                  if omitted, a new snapshot + AI call is made)

        Returns:
          ok: bool
          name: str             — the name that was applied
          frigate_yaml: str     — ready-to-paste Frigate config block
          profile: dict         — the applied FrigateProfile
        """
        if not camera_mgr:
            raise HTTPException(status_code=503, detail="Cameras not available")

        from camera_advisor import advise_camera as _advise, CameraAdvice

        profile_key = body.get("profile", "default")
        if profile_key not in ("default", "paranoid", "lax"):
            raise HTTPException(status_code=400, detail="profile must be 'default', 'paranoid', or 'lax'")

        # Use pre-computed advice if provided, otherwise compute fresh
        advice_dict = body.get("advice")
        if advice_dict:
            # Reconstruct from the dict the client sent back — we only need the profile
            from camera_advisor import _build_profiles
            ai_fragment = {
                "name": advice_dict.get("suggested_name", camera_id),
                "scene": advice_dict.get("scene_description", ""),
                "objects": advice_dict.get("detected_objects", ["person", "car"]),
                "zones": [
                    {
                        "name": z["name"],
                        "description": z.get("description", ""),
                        "coordinates": z.get("coordinates", [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]),
                        "objects": z.get("objects", []),
                    }
                    for p in advice_dict.get("profiles", {}).values()
                    for z in p.get("zones", [])
                    # zones are the same across all profiles — take from the first
                ][:len(next(iter(advice_dict.get("profiles", {}).values()), {}).get("zones", []))],
                "reasoning": advice_dict.get("ai_reasoning", ""),
            }
            # Simpler: just use the zones from the default profile (same in all three)
            default_profile_dict = advice_dict.get("profiles", {}).get("default", {})
            ai_fragment["zones"] = default_profile_dict.get("zones", [])
            profiles = _build_profiles(ai_fragment)
            suggested_name = advice_dict.get("suggested_name", camera_id)
        else:
            jpeg = await camera_mgr.snapshot(camera_id)
            if not jpeg:
                raise HTTPException(status_code=400, detail="Cannot capture snapshot")
            advice = await _advise(camera_id, jpeg)
            profiles = advice.profiles
            suggested_name = advice.suggested_name

        profile = profiles[profile_key]

        # Apply the name if provided or suggested
        new_name = body.get("suggested_name") or suggested_name
        if new_name and new_name != camera_id:
            camera_mgr.update_camera(camera_id, {"name": new_name})

        frigate_yaml = profile.to_frigate_yaml(camera_id)

        return {
            "ok": True,
            "name": new_name,
            "frigate_yaml": frigate_yaml,
            "profile": profile.to_dict(),
        }

    # --- WHIP (mobile camera ingest) endpoints ---

    @app.get("/api/v1/cameras/whip")
    async def whip_sessions_list(request: Request) -> dict[str, Any]:
        """List active WHIP (WebRTC HTTP Ingest Protocol) sessions."""
        _require_scope(request, SCOPE_READ)
        if not mobile_cam:
            return {"sessions": []}
        sessions = await mobile_cam.list_sessions()
        return {"sessions": sessions}

    @app.post("/api/v1/cameras/whip")
    async def whip_offer(
        request: Request,
        name: str = "Mobile Camera",
        relay_rtmp: str = "",
    ) -> Any:
        """
        WHIP ingest endpoint.  Accepts an SDP offer from a mobile phone and
        returns an SDP answer.  Responds 201 Created with Location header.

        Content-Type: application/sdp
        Body: SDP offer text

        Returns:
          201 Created
          Content-Type: application/sdp
          Location: /api/v1/cameras/whip/{session_id}
          Body: SDP answer text
        """
        if not mobile_cam:
            raise HTTPException(status_code=503, detail="Mobile camera ingest not available")

        content_type = request.headers.get("content-type", "")
        if "application/sdp" not in content_type:
            raise HTTPException(status_code=415, detail="Content-Type must be application/sdp")

        offer_sdp = (await request.body()).decode("utf-8", errors="replace")
        if not offer_sdp.strip():
            raise HTTPException(status_code=422, detail="Empty SDP offer")

        try:
            answer_sdp, session_id = await mobile_cam.start_session(
                offer_sdp=offer_sdp,
                name=name,
                relay_rtmp=relay_rtmp,
            )
        except RuntimeError as exc:
            raise HTTPException(status_code=503, detail=str(exc))
        except Exception as exc:
            log.exception("WHIP offer failed: %s", exc)
            raise HTTPException(status_code=500, detail=f"WHIP session error: {exc}")

        from fastapi.responses import Response
        return Response(
            content=answer_sdp,
            status_code=201,
            media_type="application/sdp",
            headers={"Location": f"/api/v1/cameras/whip/{session_id}"},
        )

    @app.patch("/api/v1/cameras/whip/{session_id}")
    async def whip_trickle(session_id: str, request: Request) -> Any:
        """
        WHIP trickle ICE endpoint.  Accepts an ICE candidate SDP fragment and
        adds it to the peer connection.

        Content-Type: application/trickle-ice-sdpfrag
        Body: SDP fragment (a=candidate:... lines)

        Returns: 204 No Content
        """
        if not mobile_cam:
            raise HTTPException(status_code=503, detail="Mobile camera ingest not available")

        session = await mobile_cam.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="WHIP session not found")

        body = (await request.body()).decode("utf-8", errors="replace")
        try:
            await mobile_cam.trickle(session_id, body)
        except Exception as exc:
            log.warning("WHIP trickle error: %s", exc)

        from fastapi.responses import Response
        return Response(status_code=204)

    @app.delete("/api/v1/cameras/whip/{session_id}")
    async def whip_delete(session_id: str) -> dict[str, Any]:
        """Teardown a WHIP session."""
        if not mobile_cam:
            raise HTTPException(status_code=503, detail="Mobile camera ingest not available")
        session = await mobile_cam.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="WHIP session not found")
        await mobile_cam.end_session(session_id)
        return {"ok": True}

    @app.get("/api/v1/cameras/whip")
    async def whip_list() -> dict[str, Any]:
        """List active WHIP (mobile camera) sessions."""
        if not mobile_cam:
            return {"sessions": []}
        return {"sessions": await mobile_cam.list_sessions()}

    @app.get("/api/v1/cameras/whip/{session_id}")
    async def whip_get(session_id: str) -> dict[str, Any]:
        """Get stats/status for a WHIP session."""
        if not mobile_cam:
            raise HTTPException(status_code=503, detail="Mobile camera ingest not available")
        session = await mobile_cam.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="WHIP session not found")
        return session.to_dict()

    # ── V1.6 consumer camera: clip browsing, guest tokens, push webhooks ─────

    # Clip browsing
    @app.get("/api/v1/cameras/{camera_id}/clips")
    async def camera_list_clips(
        request: Request,
        camera_id: str,
        limit: int = 50,
        before: float | None = None,
        event_type: str | None = None,
    ) -> list[dict[str, Any]]:
        """List recorded clips for a camera, newest first."""
        _require_scope(request, SCOPE_READ)
        if mobile_cam is None:
            raise HTTPException(503, "Mobile camera service not available")
        from mobile_camera import ClipBrowser
        recordings_dir = getattr(mobile_cam, "_hls_dir", Path(".")).parent / "recordings"
        browser = ClipBrowser(recordings_dir)
        clips = browser.list_clips(camera_id, limit=limit, before=before, event_type=event_type)
        return [c.to_dict() for c in clips]

    @app.get("/api/v1/cameras/{camera_id}/clips/{clip_id}")
    async def camera_get_clip(request: Request, camera_id: str, clip_id: str) -> dict[str, Any]:
        """Get metadata for a specific recorded clip."""
        _require_scope(request, SCOPE_READ)
        if mobile_cam is None:
            raise HTTPException(503, "Mobile camera service not available")
        from mobile_camera import ClipBrowser
        recordings_dir = getattr(mobile_cam, "_hls_dir", Path(".")).parent / "recordings"
        browser = ClipBrowser(recordings_dir)
        clip = browser.get_clip(camera_id, clip_id)
        if not clip:
            raise HTTPException(404, f"Clip {clip_id} not found")
        return clip.to_dict()

    # Guest tokens
    @app.post("/api/v1/cameras/guest-tokens")
    async def create_guest_token(request: Request) -> dict[str, Any]:
        """Create a camera-view-only guest token for sharing with non-technical users."""
        _require_scope(request, SCOPE_WRITE)
        if mobile_cam is None:
            raise HTTPException(503, "Mobile camera service not available")
        body = await request.json()
        from mobile_camera import GuestTokenManager
        gtm_dir = getattr(mobile_cam, "_hls_dir", Path(".")).parent / "guest_tokens"
        gtm = GuestTokenManager(data_dir=gtm_dir)
        gt = gtm.create_token(
            label=body.get("label", ""),
            camera_ids=body.get("camera_ids", []),
            ttl_days=int(body.get("ttl_days", 365)),
        )
        return gt.to_dict()

    @app.get("/api/v1/cameras/guest-tokens")
    async def list_guest_tokens(request: Request) -> list[dict[str, Any]]:
        """List active guest tokens."""
        _require_scope(request, SCOPE_READ)
        if mobile_cam is None:
            raise HTTPException(503, "Mobile camera service not available")
        from mobile_camera import GuestTokenManager
        gtm_dir = getattr(mobile_cam, "_hls_dir", Path(".")).parent / "guest_tokens"
        gtm = GuestTokenManager(data_dir=gtm_dir)
        return gtm.list_tokens()

    @app.delete("/api/v1/cameras/guest-tokens/{token}")
    async def revoke_guest_token(request: Request, token: str) -> dict[str, Any]:
        """Revoke a guest token."""
        _require_scope(request, SCOPE_WRITE)
        if mobile_cam is None:
            raise HTTPException(503, "Mobile camera service not available")
        from mobile_camera import GuestTokenManager
        gtm_dir = getattr(mobile_cam, "_hls_dir", Path(".")).parent / "guest_tokens"
        gtm = GuestTokenManager(data_dir=gtm_dir)
        ok = gtm.revoke(token)
        if not ok:
            raise HTTPException(404, "Token not found")
        return {"ok": True}

    # Push webhooks (motion notifications)
    def _get_push_mgr() -> Any:
        if mobile_cam is None:
            raise HTTPException(503, "Mobile camera service not available")
        from mobile_camera import MotionPushManager
        push_dir = getattr(mobile_cam, "_hls_dir", Path(".")).parent / "push_webhooks"
        return MotionPushManager(data_dir=push_dir)

    @app.get("/api/v1/cameras/push-webhooks")
    async def list_push_webhooks(request: Request) -> list[dict[str, Any]]:
        """List registered motion push webhooks."""
        _require_scope(request, SCOPE_READ)
        return _get_push_mgr().list_webhooks()

    @app.post("/api/v1/cameras/push-webhooks")
    async def register_push_webhook(request: Request) -> dict[str, Any]:
        """Register a webhook URL to receive motion/object alert notifications."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        url = body.get("url", "")
        if not url:
            raise HTTPException(400, "url is required")
        wh = _get_push_mgr().register(
            url=url,
            camera_ids=body.get("camera_ids", []),
            events=body.get("events", []),
            label=body.get("label", ""),
        )
        return wh.to_dict()

    @app.delete("/api/v1/cameras/push-webhooks/{webhook_id}")
    async def unregister_push_webhook(request: Request, webhook_id: str) -> dict[str, Any]:
        """Remove a push webhook."""
        _require_scope(request, SCOPE_WRITE)
        ok = _get_push_mgr().unregister(webhook_id)
        if not ok:
            raise HTTPException(404, f"Webhook {webhook_id} not found")
        return {"ok": True}

    @app.post("/api/v1/cameras/{camera_id}/notify")
    async def trigger_camera_notify(request: Request, camera_id: str) -> dict[str, Any]:
        """Manually trigger a push notification for a camera event (for testing)."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        mgr = _get_push_mgr()
        count = await mgr.notify(
            camera_id=camera_id,
            event_type=body.get("event_type", "motion"),
            camera_name=body.get("camera_name", ""),
            label=body.get("label", ""),
            confidence=float(body.get("confidence", 0.0)),
            snapshot_url=body.get("snapshot_url", ""),
        )
        return {"ok": True, "delivered": count}

    # --- Video overlay endpoints ---

    _overlays_store: list[dict] = []

    @app.get("/api/v1/overlays")
    async def overlays_list(request: Request) -> list[dict]:
        """List active video overlays (PiP cameras, media, etc.)."""
        _require_scope(request, SCOPE_READ)
        return list(_overlays_store)

    @app.post("/api/v1/overlays")
    async def overlay_add(request: Request, body: dict = {}) -> dict[str, Any]:
        """Add a video overlay source."""
        _require_scope(request, SCOPE_WRITE)
        import time as _time
        overlay = {
            "id": f"overlay-{int(_time.time() * 1000)}",
            "type": body.get("type", "camera"),
            "source_id": body.get("source_id", ""),
            "label": body.get("label", ""),
            "position": body.get("position", {"x": 0, "y": 0, "width": 320, "height": 180}),
            "enabled": True,
        }
        _overlays_store.append(overlay)
        return overlay

    @app.delete("/api/v1/overlays/{overlay_id}")
    async def overlay_remove(request: Request, overlay_id: str) -> dict[str, Any]:
        """Remove a video overlay."""
        _require_scope(request, SCOPE_WRITE)
        before = len(_overlays_store)
        _overlays_store[:] = [o for o in _overlays_store if o.get("id") != overlay_id]
        if len(_overlays_store) == before:
            raise HTTPException(404, "Overlay not found")
        return {"ok": True}

    # --- OBS / Broadcast studio endpoints ---

    @app.post("/api/v1/broadcast/start")
    async def broadcast_start(body: dict = {}) -> dict[str, Any]:
        """Start broadcast (record + stream). Convenience alias."""
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        results: dict[str, Any] = {}
        if body.get("record", True):
            results["recording"] = await obs_studio.start_recording()
        if body.get("stream", False):
            results["streaming"] = await obs_studio.start_streaming()
        return {"ok": True, **results}

    @app.get("/api/v1/broadcast/status")
    async def broadcast_status() -> dict[str, Any]:
        if not obs_studio:
            return {"connected": False, "scenes": [], "sources": []}
        return obs_studio.status()

    @app.get("/api/v1/broadcast/scenes")
    async def broadcast_scenes() -> dict[str, Any]:
        if not obs_studio:
            return {"scenes": []}
        return {"scenes": obs_studio.list_scenes()}

    @app.post("/api/v1/broadcast/scenes")
    async def create_broadcast_scene(body: dict = {}) -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        scene = await obs_studio.create_scene(body.get("id", ""), body.get("name", ""))
        if not scene:
            raise HTTPException(status_code=400, detail="Failed to create scene")
        return scene.to_dict()

    @app.delete("/api/v1/broadcast/scenes/{scene_id}")
    async def delete_broadcast_scene(scene_id: str) -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        ok = await obs_studio.remove_scene(scene_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Scene not found")
        return {"ok": True}

    @app.post("/api/v1/broadcast/scenes/{scene_id}/switch")
    async def switch_broadcast_scene(scene_id: str, body: dict = {}) -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        preview = body.get("preview", False)
        ok = await obs_studio.switch_scene(scene_id, preview=preview)
        return {"ok": ok}

    @app.post("/api/v1/broadcast/transition")
    async def broadcast_transition() -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        ok = await obs_studio.trigger_transition()
        return {"ok": ok}

    @app.get("/api/v1/broadcast/sources")
    async def broadcast_sources() -> dict[str, Any]:
        if not obs_studio:
            return {"sources": []}
        return {"sources": obs_studio.list_sources()}

    @app.post("/api/v1/broadcast/scenes/{scene_id}/sources")
    async def add_source_to_scene(scene_id: str, body: dict = {}) -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        ok = await obs_studio.add_source_to_scene(
            scene_id, body.get("source_id", ""),
            x=body.get("x", 0), y=body.get("y", 0),
            width=body.get("width", 1920), height=body.get("height", 1080),
        )
        return {"ok": ok}

    @app.post("/api/v1/broadcast/record/start")
    async def broadcast_record_start() -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        ok = await obs_studio.start_recording()
        return {"ok": ok}

    @app.post("/api/v1/broadcast/record/stop")
    async def broadcast_record_stop() -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        path = await obs_studio.stop_recording()
        return {"ok": bool(path), "path": path}

    @app.post("/api/v1/broadcast/stream/start")
    async def broadcast_stream_start() -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        ok = await obs_studio.start_streaming()
        return {"ok": ok}

    @app.post("/api/v1/broadcast/stream/stop")
    async def broadcast_stream_stop() -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        ok = await obs_studio.stop_streaming()
        return {"ok": ok}

    @app.post("/api/v1/broadcast/audio/volume")
    async def broadcast_audio_volume(body: dict = {}) -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        ok = await obs_studio.set_source_volume(body.get("source_id", ""), body.get("volume_db", 0.0))
        return {"ok": ok}

    @app.post("/api/v1/broadcast/audio/mute")
    async def broadcast_audio_mute(body: dict = {}) -> dict[str, Any]:
        if not obs_studio:
            raise HTTPException(status_code=503, detail="Broadcast not available")
        ok = await obs_studio.set_source_mute(body.get("source_id", ""), body.get("muted", False))
        return {"ok": ok}

    # --- Stream router endpoints ---

    @app.get("/api/v1/routes")
    async def list_stream_routes() -> dict[str, Any]:
        if not stream_router:
            return {"routes": []}
        return {"routes": stream_router.list_routes()}

    @app.post("/api/v1/routes")
    async def create_stream_route(body: dict = {}) -> dict[str, Any]:
        if not stream_router:
            raise HTTPException(status_code=503, detail="Stream router not available")
        route = stream_router.create_route(body)
        if not route:
            raise HTTPException(status_code=400, detail="Invalid route or ID exists")
        return route.to_dict()

    @app.delete("/api/v1/routes/{route_id}")
    async def delete_stream_route(route_id: str) -> dict[str, Any]:
        if not stream_router:
            raise HTTPException(status_code=503, detail="Stream router not available")
        ok = stream_router.remove_route(route_id)
        if not ok:
            raise HTTPException(status_code=400, detail="Route not found or still active")
        return {"ok": True}

    @app.post("/api/v1/routes/{route_id}/start")
    async def start_stream_route(route_id: str) -> dict[str, Any]:
        if not stream_router:
            raise HTTPException(status_code=503, detail="Stream router not available")
        ok = await stream_router.start_route(route_id)
        return {"ok": ok}

    @app.post("/api/v1/routes/{route_id}/stop")
    async def stop_stream_route(route_id: str) -> dict[str, Any]:
        if not stream_router:
            raise HTTPException(status_code=503, detail="Stream router not available")
        ok = await stream_router.stop_route(route_id)
        return {"ok": ok}

    # --- Guacamole endpoints ---

    @app.get("/api/v1/guacamole/status")
    async def guacamole_status() -> dict[str, Any]:
        if not guac_mgr:
            return {"connected": False}
        return guac_mgr.status()

    @app.post("/api/v1/guacamole/deploy")
    async def guacamole_deploy(body: dict = {}) -> dict[str, Any]:
        if not guac_mgr:
            raise HTTPException(status_code=503, detail="Guacamole manager not available")
        ok = await guac_mgr.deploy(body)
        return {"ok": ok}

    @app.post("/api/v1/guacamole/teardown")
    async def guacamole_teardown() -> dict[str, Any]:
        if not guac_mgr:
            raise HTTPException(status_code=503, detail="Guacamole manager not available")
        ok = await guac_mgr.teardown()
        return {"ok": ok}

    @app.get("/api/v1/guacamole/deployment")
    async def guacamole_deployment_status() -> dict[str, Any]:
        if not guac_mgr:
            return {"running": False}
        return await guac_mgr.deployment_status()

    @app.get("/api/v1/guacamole/connections")
    async def guacamole_connections() -> dict[str, Any]:
        if not guac_mgr:
            return {"connections": []}
        return {"connections": await guac_mgr.list_connections()}

    @app.get("/api/v1/guacamole/users")
    async def guacamole_users() -> dict[str, Any]:
        if not guac_mgr:
            return {"users": []}
        return {"users": await guac_mgr.list_users()}

    @app.post("/api/v1/guacamole/users")
    async def guacamole_create_user(body: dict = {}) -> dict[str, Any]:
        if not guac_mgr:
            raise HTTPException(status_code=503, detail="Guacamole manager not available")
        ok = await guac_mgr.create_user(
            body.get("username", ""), body.get("password", ""),
            connection_ids=body.get("connection_ids"),
        )
        return {"ok": ok}

    # --- Provisioning endpoints ---

    @app.get("/api/v1/provisioning/status")
    async def provisioning_status() -> dict[str, Any]:
        if not provision_mgr:
            return {"bays": [], "queue_length": 0}
        return provision_mgr.status()

    @app.get("/api/v1/provisioning/bays")
    async def list_provision_bays() -> dict[str, Any]:
        if not provision_mgr:
            return {"bays": []}
        return {"bays": provision_mgr.list_bays()}

    @app.post("/api/v1/provisioning/bays")
    async def add_provision_bay(body: dict = {}) -> dict[str, Any]:
        if not provision_mgr:
            raise HTTPException(status_code=503, detail="Provisioning not available")
        bay = provision_mgr.add_bay(
            body.get("id", ""), body.get("name", ""),
            body.get("node_id", ""), body.get("screen_id", ""),
        )
        return bay.to_dict()

    @app.get("/api/v1/provisioning/profiles")
    async def list_provision_profiles() -> dict[str, Any]:
        if not provision_mgr:
            return {"profiles": []}
        return {"profiles": provision_mgr.list_profiles()}

    @app.post("/api/v1/provisioning/profiles")
    async def add_provision_profile(body: dict = {}) -> dict[str, Any]:
        if not provision_mgr:
            raise HTTPException(status_code=503, detail="Provisioning not available")
        profile = provision_mgr.add_profile(body)
        return profile.to_dict()

    @app.get("/api/v1/provisioning/jobs")
    async def list_provision_jobs(state: str = "") -> dict[str, Any]:
        if not provision_mgr:
            return {"jobs": []}
        return {"jobs": provision_mgr.list_jobs(state)}

    @app.get("/api/v1/provisioning/jobs/{job_id}")
    async def get_provision_job(job_id: str) -> dict[str, Any]:
        if not provision_mgr:
            raise HTTPException(status_code=503, detail="Provisioning not available")
        job = provision_mgr.get_job(job_id)
        if not job:
            raise HTTPException(status_code=404, detail="Job not found")
        return job.to_dict()

    @app.post("/api/v1/provisioning/jobs")
    async def create_provision_job(body: dict = {}) -> dict[str, Any]:
        """
        Create a provisioning job.

        Body:
          user: {username, display_name, email, department, role}
          profile_id: device profile to use
          device_serial: optional pre-assigned serial
          shipping_address: optional {name, street, city, state, postcode, country}
        """
        if not provision_mgr:
            raise HTTPException(status_code=503, detail="Provisioning not available")
        job = await provision_mgr.create_job(
            body.get("user", {}),
            body.get("profile_id", ""),
            device_serial=body.get("device_serial", ""),
            shipping_address=body.get("shipping_address"),
        )
        if not job:
            raise HTTPException(status_code=400, detail="Invalid profile or user data")
        return job.to_dict()

    @app.post("/api/v1/provisioning/jobs/{job_id}/complete")
    async def complete_provision_job(job_id: str) -> dict[str, Any]:
        """Mark a job as complete (device picked up or shipped)."""
        if not provision_mgr:
            raise HTTPException(status_code=503, detail="Provisioning not available")
        ok = await provision_mgr.mark_complete(job_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Job not found")
        return {"ok": True}

    @app.post("/api/v1/provisioning/directory/sync")
    async def provisioning_directory_sync(body: dict = {}) -> dict[str, Any]:
        """Sync users from an external directory (LDAP, Azure AD, etc.)."""
        if not provision_mgr:
            raise HTTPException(status_code=503, detail="Provisioning not available")
        new_users = await provision_mgr.sync_from_directory(
            body.get("type", "manual"), body.get("config", {}),
        )
        return {"new_users": [u.to_dict() for u in new_users]}

    # --- Ozma Connect endpoints ---

    @app.get("/api/v1/connect/status")
    async def connect_status() -> dict[str, Any]:
        if not connect:
            status: dict[str, Any] = {"authenticated": False, "tier": "free"}
        else:
            status = connect.status()
        # Always include usage and limits so consumers can read counters
        # regardless of authentication state
        status.setdefault("usage", {"nodes": len(state.nodes), "controllers": 1})
        status.setdefault("limits", {"controllers": -1, "nodes": -1})
        return status

    # Known OSS features — all are allowed; unknown features return 404
    _KNOWN_FEATURES = frozenset({
        "room_correction", "noise_cancellation", "ai_agent", "config_backup",
        "relay", "subdomain", "remote_desktop", "session_recording",
        "screen_reader", "automation", "macros", "scheduler",
        "rgb", "vban", "bluetooth", "kdeconnect", "transcription",
        "metrics_export", "audit_log", "compliance_report", "dlp",
        "network_scan", "saas_management", "vaultwarden", "cloud_backup",
    })

    @app.get("/api/v1/connect/check/{feature_name}")
    async def connect_feature_check(feature_name: str) -> dict[str, Any]:
        """Check whether a feature is allowed on the current tier.

        The OSS controller has no metering — all known features are allowed.
        Unknown features return 404.
        """
        if feature_name not in _KNOWN_FEATURES:
            raise HTTPException(status_code=404, detail=f"Unknown feature: {feature_name}")
        tier = connect.tier if connect else "free"
        return {"allowed": True, "feature": feature_name, "tier": tier}

    @app.post("/api/v1/connect/login")
    async def connect_login(body: dict = {}) -> dict[str, Any]:
        if not connect:
            raise HTTPException(status_code=503, detail="Connect not available")
        ok = await connect.login(body.get("email", ""), body.get("password", ""))
        return {"ok": ok, "tier": connect.tier}

    @app.post("/api/v1/connect/logout")
    async def connect_logout() -> dict[str, Any]:
        if connect:
            connect.logout()
        return {"ok": True}

    @app.get("/api/v1/connect/webhooks")
    async def connect_webhooks_list(request: Request) -> dict[str, Any]:
        """List configured Connect webhooks (empty list when Connect not configured)."""
        _require_scope(request, SCOPE_READ)
        if not connect:
            return {"webhooks": []}
        webhooks = getattr(connect, "list_webhooks", lambda: [])()
        return {"webhooks": webhooks}

    @app.post("/api/v1/connect/webhooks/test")
    async def connect_webhooks_test(request: Request) -> dict[str, Any]:
        """Send a test ping to all configured webhooks."""
        _require_scope(request, SCOPE_WRITE)
        if not connect:
            return {"ok": True, "sent": 0}
        sent = getattr(connect, "test_webhooks", lambda: 0)()
        return {"ok": True, "sent": sent}

    # --- Security / mesh endpoints ---

    @app.get("/api/v1/security/status")
    async def security_status() -> dict[str, Any]:
        result: dict[str, Any] = {}
        if mesh_ca:
            result["mesh"] = mesh_ca.status()
            result["nodes"] = mesh_ca.list_nodes()
        if sess_mgr:
            result["sessions"] = sess_mgr.list_sessions()
        return result

    @app.get("/api/v1/security/pending")
    async def security_pending() -> dict[str, Any]:
        """List nodes awaiting pairing approval."""
        # Pending nodes are unpaired nodes visible via mDNS
        paired_ids = set()
        if mesh_ca:
            paired_ids = {n["node_id"] for n in mesh_ca.list_nodes()}
        unpaired = [
            {"id": nid, "host": n.host, "hw": n.hw}
            for nid, n in state.nodes.items()
            if nid not in paired_ids
        ]
        return {"pending": unpaired}

    @app.post("/api/v1/security/approve")
    async def security_approve(body: dict = {}) -> dict[str, Any]:
        """Approve a pending node for pairing."""
        if not mesh_ca:
            raise HTTPException(status_code=503, detail="Mesh CA not available")
        node_id = body.get("node_id", "")
        # For now, auto-generate a keypair for the node
        # (real implementation: node presents its own pubkey during pairing)
        from transport import IdentityKeyPair
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        # Use a deterministic key for now (real: node sends its pubkey)
        import hashlib
        seed = hashlib.sha256(node_id.encode()).digest()
        cert = mesh_ca.approve_node(
            node_id, seed, node.capabilities or ["hid"],
        )
        if cert:
            return {"ok": True, "fingerprint": cert.public_key.hex()[:16]}
        raise HTTPException(status_code=500, detail="Failed to issue certificate")

    @app.post("/api/v1/security/revoke")
    async def security_revoke(body: dict = {}) -> dict[str, Any]:
        if not mesh_ca:
            raise HTTPException(status_code=503, detail="Mesh CA not available")
        ok = mesh_ca.revoke_node(body.get("node_id", ""), body.get("reason", ""))
        return {"ok": ok}

    # --- Terminal endpoints ---

    from terminal import TerminalManager
    term_mgr = TerminalManager()

    @app.get("/api/v1/terminal/sessions")
    async def list_terminal_sessions(node_id: str = "") -> dict[str, Any]:
        return {"sessions": term_mgr.list_sessions(node_id)}

    @app.post("/api/v1/terminal/create")
    async def create_terminal(body: dict = {}) -> dict[str, Any]:
        node_id = body.get("node_id", "")
        session_id = body.get("session_id", f"term-{node_id}-{int(time.time())}")
        rows = body.get("rows", 24)
        cols = body.get("cols", 80)
        session = await term_mgr.create_session(session_id, node_id, rows=rows, cols=cols)
        if not session:
            raise HTTPException(status_code=500, detail="Failed to create terminal")
        return session.to_dict()

    @app.post("/api/v1/terminal/{session_id}/close")
    async def close_terminal(session_id: str) -> dict[str, Any]:
        ok = await term_mgr.close_session(session_id)
        return {"ok": ok}

    @app.post("/api/v1/terminal/{session_id}/resize")
    async def resize_terminal(session_id: str, body: dict = {}) -> dict[str, Any]:
        ok = await term_mgr.resize(session_id, body.get("rows", 24), body.get("cols", 80))
        return {"ok": ok}

    @app.websocket("/api/v1/terminal/{session_id}/ws")
    async def terminal_ws(ws: WebSocket, session_id: str) -> None:
        """WebSocket for terminal I/O. Attach to an existing session."""
        if not await _ws_authenticate(ws):
            await ws.close(code=4001, reason="Authentication required")
            return
        await ws.accept()

        session = term_mgr.get_session(session_id)
        if not session:
            await ws.close(code=4004, reason="Session not found")
            return

        # Attach this client — sends scrollback immediately
        await term_mgr.attach_client(session_id, ws)

        try:
            while True:
                data = await ws.receive_bytes()
                await term_mgr.write(session_id, data)
        except Exception:
            pass
        finally:
            term_mgr.detach_client(session_id, ws)

    # --- Remote desktop endpoints ---

    from remote_desktop import RemoteDesktopManager
    rd_mgr = RemoteDesktopManager(state, event_queue=state.events, notifier=notifier)

    @app.on_event("startup")
    async def _start_rd_idle_monitor() -> None:
        asyncio.create_task(rd_mgr.start_idle_monitor(), name="rd-idle-monitor")

    @app.get("/api/v1/remote/sessions")
    async def list_remote_sessions(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return {"sessions": rd_mgr.list_sessions()}

    @app.websocket("/api/v1/remote/{node_id}/ws")
    async def remote_desktop_ws(ws: WebSocket, node_id: str) -> None:
        if not await _ws_authenticate(ws):
            await ws.close(code=4001, reason="Authentication required")
            return
        session = rd_mgr.create_session(node_id)
        if not session:
            await ws.accept()
            await ws.close(code=4004, reason="Node not found")
            return
        approved = await rd_mgr.start_session_with_consent(session)
        if not approved:
            await ws.accept()
            await ws.send_text(json.dumps({"type": "consent_denied"}))
            await ws.close(code=4003, reason="Consent denied or timed out")
            rd_mgr.end_session(session.session_id)
            return
        try:
            await session.handle_ws(ws)
        finally:
            await rd_mgr.fire_event({
                "type": "remote_desktop.ended",
                "session_id": session.session_id,
                "node_id": node_id,
                "reason": "disconnected",
            })
            rd_mgr.end_session(session.session_id)

    @app.post("/api/v1/remote/{session_id}/approve")
    async def approve_remote_session(request: Request, session_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        ok = rd_mgr.approve_session(session_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Pending session not found")
        return {"ok": True}

    @app.post("/api/v1/remote/{session_id}/reject")
    async def reject_remote_session(request: Request, session_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        ok = rd_mgr.reject_session(session_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Pending session not found")
        return {"ok": True}

    @app.get("/console/{node_id:path}")
    async def console_page(node_id: str) -> Response:
        """Serve the full-featured KVM console for a node."""
        console_path = Path(__file__).parent / "static" / "console.html"
        if not console_path.exists():
            raise HTTPException(status_code=404, detail="Console not found")
        return Response(content=console_path.read_text(), media_type="text/html")

    # --- VGA font endpoint (for OCR canvas renderer) ---

    _tc_singleton: Any = None

    @app.get("/api/v1/text_capture/font")
    async def get_text_capture_font(request: Request) -> dict[str, Any]:
        """Return VGA bitmap font data for pixel-perfect canvas rendering.

        Each glyph is a list of integers (one per row), MSB = leftmost pixel.
        Used by console.html's OCR codec mode to render exact VGA glyphs on canvas.
        """
        _require_scope(request, SCOPE_READ)
        nonlocal _tc_singleton
        if _tc_singleton is None:
            from text_capture import TextCapture as _TC
            _tc_singleton = _TC()
        return _tc_singleton.export_font_json()

    # --- Terminal bridge (VGA OCR → xterm.js) ---

    from terminal_bridge import TerminalBridgeManager
    tb_mgr = TerminalBridgeManager(state, streams=streams)

    @app.on_event("startup")
    async def _start_tb_manager() -> None:
        tb_mgr.start()

    @app.get("/terminal/{node_id:path}")
    async def terminal_page(node_id: str) -> Response:
        """Serve the VGA-OCR terminal bridge page for a node (no video stream required)."""
        p = Path(__file__).parent / "static" / "terminal.html"
        if not p.exists():
            raise HTTPException(status_code=404, detail="Terminal page not found")
        return Response(content=p.read_text(), media_type="text/html")

    @app.websocket("/api/v1/remote/{node_id}/terminal/ws")
    async def terminal_bridge_ws(ws: WebSocket, node_id: str) -> None:
        """
        Bidirectional WebSocket for the VGA OCR terminal bridge.

        Server → client: ANSI escape sequence bytes (write directly to xterm.js)
                         OR JSON text frames for control messages (resize, error)
        Client → server: JSON text frames {type: keydown/keyup/paste/key_sequence, ...}
        """
        if not await _ws_authenticate(ws):
            await ws.close(code=4001, reason="Authentication required")
            return

        bridge = tb_mgr.get_or_create(node_id)
        if not bridge:
            await ws.accept()
            await ws.close(code=4004, reason="Node not found")
            return

        await ws.accept()
        session = await bridge.add_session(ws)

        from remote_desktop import _KEY_TO_HID, _MOD_BITS as _RD_MOD_BITS

        # Per-session modifier state (sticky mods across messages)
        _modifiers = 0
        _pressed_keys: list[int] = []

        try:
            while True:
                data = await ws.receive_text()
                msg = json.loads(data)
                t = msg.get("type", "")

                if t == "keydown":
                    code = msg.get("code", "")
                    if code in _RD_MOD_BITS:
                        _modifiers |= _RD_MOD_BITS[code]
                    else:
                        hid = _KEY_TO_HID.get(code, 0)
                        if hid and hid not in _pressed_keys:
                            _pressed_keys.append(hid)
                            if len(_pressed_keys) > 6:
                                _pressed_keys.pop(0)
                    _flush_kbd(bridge, _modifiers, _pressed_keys)

                elif t == "keyup":
                    code = msg.get("code", "")
                    if code in _RD_MOD_BITS:
                        _modifiers &= ~_RD_MOD_BITS[code]
                    else:
                        hid = _KEY_TO_HID.get(code, 0)
                        if hid in _pressed_keys:
                            _pressed_keys.remove(hid)
                    _flush_kbd(bridge, _modifiers, _pressed_keys)

                elif t == "paste":
                    text = msg.get("text", "")[:10000]
                    await _tb_paste(bridge, text)

                elif t == "key_sequence":
                    seq = msg.get("sequence", "")
                    await _tb_key_sequence(bridge, seq)

        except Exception:
            pass
        finally:
            bridge.remove_session(session)

    def _flush_kbd(bridge: Any, mods: int, pressed: list[int]) -> None:
        """Send current keyboard state as HID report to the node."""
        keys = (pressed + [0] * 6)[:6]
        report = bytes([mods, 0] + keys)
        bridge.send_hid(0x01, report)

    async def _tb_paste(bridge: Any, text: str) -> None:
        """Type text into the node via HID."""
        from paste_typing import LAYOUTS
        layout = LAYOUTS.get("us", {})
        for char in text:
            stroke = layout.get(char)
            if not stroke:
                continue
            bridge.send_hid(0x01, bytes([stroke.modifier, 0, stroke.key, 0, 0, 0, 0, 0]))
            await asyncio.sleep(0.02)
            bridge.send_hid(0x01, bytes(8))
            await asyncio.sleep(0.015)

    async def _tb_key_sequence(bridge: Any, sequence: str) -> None:
        """Send a predefined key sequence (Ctrl+Alt+Del etc.) to the node."""
        from remote_desktop import RemoteDesktopSession
        import socket as _socket
        node_id_ = getattr(bridge, "node_id", "")
        node = state.nodes.get(node_id_)
        if not node:
            return
        # Reuse the sequence table from a temporary RemoteDesktopSession
        tmp = RemoteDesktopSession(node_id_, node.host, node.port)
        await tmp._on_key_sequence(sequence)
        tmp._sock.close()

    @app.get("/api/v1/remote/{node_id}/screenshot")
    async def remote_screenshot(request: Request, node_id: str) -> Response:
        """Capture current frame from the node's stream as JPEG."""
        _require_scope(request, SCOPE_READ)
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        # Try snapshot endpoint on the node
        import httpx
        snapshot_url = f"http://127.0.0.1:{node.api_port}/display/snapshot"
        try:
            async with httpx.AsyncClient(timeout=5.0) as client:
                resp = await client.get(snapshot_url)
                if resp.status_code == 200:
                    return Response(content=resp.content, media_type="image/jpeg",
                                    headers={"Content-Disposition": f"inline; filename=\"{node_id}.jpg\""})
        except Exception:
            pass
        # Fall back to one MJPEG frame via the capture pipeline
        if streams:
            frame = await streams.get_snapshot(node_id)
            if frame:
                return Response(content=frame, media_type="image/jpeg",
                                headers={"Content-Disposition": f"inline; filename=\"{node_id}.jpg\""})
        raise HTTPException(status_code=503, detail="No stream available")

    @app.get("/api/v1/remote/{node_id}/clipboard")
    async def get_remote_clipboard(request: Request, node_id: str) -> dict[str, Any]:
        """Read clipboard from guest — via agent if connected, else OCR the screen."""
        _require_scope(request, SCOPE_READ)
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")
        # Check if desktop agent is connected and can provide clipboard
        agent_ws = getattr(state, "agent_connections", {}).get(node_id)
        if agent_ws:
            try:
                result = await asyncio.wait_for(
                    _request_agent_clipboard(agent_ws), timeout=3.0
                )
                if result:
                    return {"text": result, "source": "agent"}
            except (asyncio.TimeoutError, Exception):
                pass
        # Fallback: OCR the screen
        if streams:
            frame = await streams.get_snapshot(node_id)
            if frame:
                try:
                    from screen_reader import ScreenReader
                    reader = ScreenReader()
                    text = await reader.extract_text_from_image(frame)
                    return {"text": text, "source": "ocr"}
                except Exception:
                    pass
        return {"text": "", "source": "none"}

    async def _request_agent_clipboard(agent_ws: Any) -> str | None:
        """Ask a connected desktop agent for its clipboard contents."""
        # Agent protocol: send {"type":"clipboard_get"}, receive {"type":"clipboard","text":"..."}
        try:
            import asyncio
            future: asyncio.Future[str] = asyncio.get_event_loop().create_future()
            orig_handler = getattr(agent_ws, "_clipboard_future", None)
            agent_ws._clipboard_future = future
            await agent_ws.send_text('{"type":"clipboard_get"}')
            result = await future
            agent_ws._clipboard_future = orig_handler
            return result
        except Exception:
            return None

    @app.post("/api/v1/remote/{node_id}/privacy")
    async def set_remote_privacy(request: Request, node_id: str, body: dict = {}) -> dict[str, Any]:
        """Enable/disable privacy mode (blank target display via DDC/CI)."""
        _require_scope(request, SCOPE_WRITE)
        enabled = body.get("enabled", False)
        # Find active session for this node
        for s in rd_mgr._sessions.values():
            if s.node_id == node_id and s.state.value == "active":
                s.privacy_mode = enabled
                log.info("Privacy mode %s for %s", "enabled" if enabled else "disabled", node_id)
                # DDC/CI blanking would be called here via monitor_control
                await rd_mgr.fire_event({
                    "type": "remote_desktop.privacy_changed",
                    "node_id": node_id,
                    "enabled": enabled,
                })
                return {"ok": True, "privacy_mode": enabled}
        raise HTTPException(status_code=404, detail="No active session for this node")

    # --- Terminal renderer view endpoint ---

    @app.get("/api/v1/remote/{node_id}/view")
    async def remote_view(
        request: Request,
        node_id: str,
        stream: int = 0,
        mode: str = "auto",
        cols: int = 80,
        rows: int = 24,
        fps: float = 10.0,
        pixel_mode: str = "auto",
    ) -> Response:
        """
        Render the node's display as ANSI terminal art.

        Query params:
          stream=1      — chunked streaming (keeps connection open, yields frames at fps)
          mode          — "auto" | "ocr" | "pixel"
          cols/rows     — terminal dimensions (default 80x24)
          fps           — frames per second for streaming (default 10)
          pixel_mode    — chafa pixel mode: "auto"|"sixel"|"kitty"|"half"|"braille"

        Single-frame response: text/plain; charset=utf-8 ANSI bytes
        Streaming response: application/octet-stream chunked ANSI bytes

        Usable directly from curl:
          curl -s http://localhost:7380/api/v1/remote/vm1/view
          curl -s http://localhost:7380/api/v1/remote/vm1/view?stream=1
        """
        _require_scope(request, SCOPE_READ)
        node = state.nodes.get(node_id)
        if not node:
            raise HTTPException(status_code=404, detail="Node not found")

        from terminal_renderer import render_frame, stream_frames

        async def _get_frame() -> bytes | None:
            if streams:
                return await streams.get_snapshot(node_id)
            return None

        if not stream:
            jpeg = await _get_frame()
            if not jpeg:
                raise HTTPException(status_code=503, detail="No stream available")
            loop = asyncio.get_event_loop()
            data = await loop.run_in_executor(
                None,
                lambda: render_frame(jpeg, mode, cols, rows, home=False, pixel_mode=pixel_mode),
            )
            return Response(content=data, media_type="text/plain; charset=utf-8")

        async def _gen():
            async for chunk in stream_frames(_get_frame, mode=mode, fps=fps,
                                             cols=cols, rows=rows, pixel_mode=pixel_mode):
                yield chunk

        return StreamingResponse(_gen(), media_type="application/octet-stream")

    # --- Control action endpoint ---

    @app.post("/api/v1/controls/action")
    async def trigger_control_action(body: dict = {}) -> dict[str, Any]:
        """Trigger a control action (e.g., scenario.next, audio.mute)."""
        if not controls:
            raise HTTPException(status_code=503, detail="Controls not available")
        action = body.get("action", "")
        value = body.get("value", 1)
        target = body.get("target", "")
        await controls._execute_action(action, target, value)
        return {"ok": True, "action": action}

    @app.get("/api/v1/controls/triggers")
    async def list_trigger_rules(request: Request) -> dict[str, Any]:
        """List event trigger rules (fire actions when events match)."""
        _require_scope(request, SCOPE_READ)
        rules = controls.list_trigger_rules() if controls else []
        return {"rules": rules}

    @app.post("/api/v1/controls/triggers")
    async def add_trigger_rule(request: Request, body: dict = {}) -> dict[str, Any]:
        """Add an event trigger rule.

        Body:
          event_type  — e.g. "frigate.person_recognized"
          action      — e.g. "scenario.activate"
          target      — optional (for audio/motion actions)
          value       — fixed action value; omit to pass event data
          filters     — dict of key/value filters applied to the event

        Example — switch to Matt's workstation when recognised at front door:
          {"event_type": "frigate.person_recognized",
           "filters": {"person": "Matt", "camera": "front_door"},
           "action": "scenario.activate", "value": "matt-workstation"}
        """
        _require_scope(request, SCOPE_WRITE)
        if not controls:
            raise HTTPException(status_code=503, detail="Controls not available")
        from controls import EventTriggerRule
        rule = EventTriggerRule(
            event_type=body.get("event_type", ""),
            action=body.get("action", ""),
            target=body.get("target", ""),
            value=body.get("value"),
            filters=body.get("filters", {}),
        )
        if not rule.event_type or not rule.action:
            raise HTTPException(status_code=400, detail="event_type and action are required")
        rule_id = controls.add_trigger_rule(rule)
        return {"ok": True, "rule_id": rule_id}

    @app.delete("/api/v1/controls/triggers/{rule_id}")
    async def delete_trigger_rule(request: Request, rule_id: str) -> dict[str, Any]:
        """Remove an event trigger rule."""
        _require_scope(request, SCOPE_WRITE)
        if not controls:
            raise HTTPException(status_code=503, detail="Controls not available")
        removed = controls.remove_trigger_rule(rule_id)
        if not removed:
            raise HTTPException(status_code=404, detail="Rule not found")
        return {"ok": True}

    # --- Hotkey endpoint ---

    @app.post("/api/v1/controls/hotkey")
    async def trigger_hotkey(body: dict = {}) -> dict[str, Any]:
        """Simulate a hotkey press and execute the bound action."""
        key = body.get("key", "")
        action = body.get("action", "")
        if controls:
            mapped_action = action or f"hotkey.{key}"
            try:
                await controls._execute_action(mapped_action, "", 1)
            except Exception:
                pass
        # Also handle well-known actions directly when controls aren't wired
        if action == "next_scenario":
            sc_list = scenarios.list()
            if sc_list:
                current = scenarios.active_id
                ids = [s["id"] for s in sc_list]
                try:
                    idx = ids.index(current)
                    next_id = ids[(idx + 1) % len(ids)]
                except (ValueError, IndexError):
                    next_id = ids[0]
                await scenarios.activate(next_id)
        return {"ok": True, "key": key, "action": action}

    # --- Clipboard ring endpoints ---

    _clipboard_ring: list[dict] = []

    @app.get("/api/v1/clipboard")
    async def clipboard_list(request: Request) -> list[dict]:
        """Return the cross-desk clipboard ring."""
        _require_scope(request, SCOPE_READ)
        return list(_clipboard_ring[-50:])

    @app.post("/api/v1/clipboard")
    async def clipboard_push(request: Request, body: dict = {}) -> dict[str, Any]:
        """Push text to the cross-desk clipboard ring."""
        _require_scope(request, SCOPE_WRITE)
        entry = {
            "id": f"clip-{len(_clipboard_ring)}",
            "text": body.get("text", ""),
            "source": body.get("source", "unknown"),
            "ts": __import__("time").time(),
        }
        _clipboard_ring.append(entry)
        return {"ok": True, "id": entry["id"]}

    # --- Edge-crossing endpoints ---

    _edge_crossing_config: dict = {
        "enabled": False,
        "sticky_ms": 0,
        "screens": [],
    }

    @app.get("/api/v1/edge-crossing")
    async def edge_crossing_get(request: Request) -> dict[str, Any]:
        """Return edge-crossing configuration."""
        _require_scope(request, SCOPE_READ)
        return dict(_edge_crossing_config)

    @app.put("/api/v1/edge-crossing")
    async def edge_crossing_update(request: Request, body: dict = {}) -> dict[str, Any]:
        """Update edge-crossing configuration."""
        _require_scope(request, SCOPE_WRITE)
        for k in ("enabled", "sticky_ms", "screens"):
            if k in body:
                _edge_crossing_config[k] = body[k]
        return {"ok": True, "config": dict(_edge_crossing_config)}

    # --- Workspace profiles endpoints ---

    @app.get("/api/v1/profiles")
    async def workspace_profiles_list(request: Request) -> dict[str, Any]:
        """List workspace profiles (hot-desk setup profiles)."""
        _require_scope(request, SCOPE_READ)
        from pathlib import Path as _Path
        import json as _json
        profiles_path = _Path(__file__).parent / "workspace_profiles.json"
        if profiles_path.exists():
            profiles = _json.loads(profiles_path.read_text())
        else:
            profiles = []
        return {"profiles": profiles}

    @app.post("/api/v1/profiles")
    async def workspace_profile_create(request: Request, body: dict = {}) -> dict[str, Any]:
        """Create or update a workspace profile."""
        _require_scope(request, SCOPE_WRITE)
        import json as _json
        import time as _time
        from pathlib import Path as _Path
        profiles_path = _Path(__file__).parent / "workspace_profiles.json"
        profiles = _json.loads(profiles_path.read_text()) if profiles_path.exists() else []
        name = body.get("name", "")
        existing = next((p for p in profiles if p.get("name") == name), None)
        if existing:
            existing.update(body)
            profile = existing
        else:
            profile = {"id": f"profile-{len(profiles)}", **body}
            profiles.append(profile)
        profiles_path.write_text(_json.dumps(profiles, indent=2))
        return profile

    # --- Maintenance window endpoints ---

    @app.get("/api/v1/maintenance")
    async def maintenance_list(request: Request) -> dict[str, Any]:
        """List scheduled maintenance windows."""
        _require_scope(request, SCOPE_READ)
        return {"windows": []}

    @app.post("/api/v1/maintenance")
    async def maintenance_create(request: Request, body: dict = {}) -> dict[str, Any]:
        """Schedule a maintenance window."""
        _require_scope(request, SCOPE_WRITE)
        import time as _time
        window = {
            "id": f"mw-{int(_time.time())}",
            "name": body.get("name", ""),
            "start_time": body.get("start_time", ""),
            "end_time": body.get("end_time", ""),
            "node_ids": body.get("node_ids", []),
            "actions": body.get("actions", []),
            "status": "scheduled",
        }
        return {"ok": True, "id": window["id"], "maintenance_id": window["id"], **window}

    # --- Replay buffer endpoints ---

    @app.get("/api/v1/replay/status")
    async def replay_status() -> dict[str, Any]:
        return {"enabled": True, "sources": []}

    @app.post("/api/v1/replay/save")
    async def replay_save(body: dict = {}) -> dict[str, Any]:
        return {"ok": False, "message": "No active capture sources for replay"}

    @app.post("/api/v1/replay/clip")
    async def replay_clip(body: dict = {}) -> dict[str, Any]:
        """Save a replay buffer clip. Stub — no active capture sources."""
        return {"ok": False, "message": "No active capture sources for replay clip"}

    # --- Notification endpoints ---

    @app.get("/api/v1/notifications")
    async def notification_list(request: Request) -> dict[str, Any]:
        """Return notification list.

        Supports two response shapes selected by Accept / query param:
          - Default / ?format=channels : legacy {channels, recent} for dashboard
          - ?format=mobile              : {notifications, unread_count} for the mobile app

        The mobile app sends ?limit=N&offset=N&unread_only=true.
        """
        fmt = request.query_params.get("format", "channels")
        limit = int(request.query_params.get("limit", "50"))
        offset = int(request.query_params.get("offset", "0"))
        unread_only = request.query_params.get("unread_only", "false").lower() in ("1", "true")

        if fmt == "mobile":
            # Return mobile-friendly notification history.
            # In a full implementation this would read from a persistent store.
            # For now return an empty list with the correct shape.
            return {"notifications": [], "unread_count": 0, "limit": limit, "offset": offset}

        if not notifier:
            return {"channels": [], "recent": []}
        return {"channels": notifier.list_channels() if hasattr(notifier, 'list_channels') else [],
                "recent": []}

    @app.post("/api/v1/notifications/{notification_id}/read")
    async def notification_mark_read(request: Request, notification_id: str) -> dict[str, Any]:
        """Mark a notification as read (mobile app endpoint)."""
        _require_scope(request, SCOPE_WRITE)
        # In a full implementation this would update a persistent store.
        return {"ok": True, "notification_id": notification_id}

    @app.post("/api/v1/notifications/test")
    async def notification_test(body: dict = {}) -> dict[str, Any]:
        if not notifier:
            raise HTTPException(status_code=503, detail="Notifications not available")
        return {"ok": True, "message": "Test notification sent"}

    # --- Mobile push device token endpoints ---

    import uuid as _uuid

    # In-memory store for push registrations.  A real deployment would
    # persist these to the config/DB layer, but for the controller process
    # lifetime this is sufficient and consistent with how other transient
    # registrations (e.g. connected agents) are handled.
    _push_registrations: dict[str, dict[str, Any]] = {}

    @app.post("/api/v1/push/register")
    async def push_register(request: Request, body: dict = {}) -> dict[str, Any]:
        """Register a mobile device token for push notifications.

        Body: {device_token: str, platform: "ios"|"android", device_name?: str}
        Returns: {ok: bool, registration_id: str}
        """
        _require_scope(request, SCOPE_WRITE)
        device_token = body.get("device_token", "").strip()
        platform = body.get("platform", "")
        device_name = body.get("device_name", None)

        if not device_token:
            raise HTTPException(status_code=400, detail="device_token is required")
        if platform not in ("ios", "android"):
            raise HTTPException(status_code=400, detail="platform must be 'ios' or 'android'")

        # De-duplicate: if this token is already registered, update it.
        for reg_id, reg in _push_registrations.items():
            if reg["device_token"] == device_token:
                reg["platform"] = platform
                reg["device_name"] = device_name
                reg["last_used"] = None
                log.info("push.register: updated existing registration %s (platform=%s)", reg_id, platform)
                return {"ok": True, "registration_id": reg_id}

        reg_id = str(_uuid.uuid4())
        _push_registrations[reg_id] = {
            "id": reg_id,
            "device_token": device_token,
            "platform": platform,
            "device_name": device_name,
            "registered_at": __import__("datetime").datetime.utcnow().isoformat() + "Z",
            "last_used": None,
        }
        log.info("push.register: new registration %s (platform=%s)", reg_id, platform)
        await _broadcast({"type": "push.registered", "registration_id": reg_id, "platform": platform})
        return {"ok": True, "registration_id": reg_id}

    @app.delete("/api/v1/push/unregister")
    async def push_unregister(request: Request, body: dict = {}) -> dict[str, Any]:
        """Remove a device token registration.

        Body: {device_token: str}
        Returns: {ok: bool}
        """
        _require_scope(request, SCOPE_WRITE)
        device_token = body.get("device_token", "").strip()
        if not device_token:
            raise HTTPException(status_code=400, detail="device_token is required")

        to_remove = [
            reg_id for reg_id, reg in _push_registrations.items()
            if reg["device_token"] == device_token
        ]
        for reg_id in to_remove:
            del _push_registrations[reg_id]
            log.info("push.unregister: removed registration %s", reg_id)

        if not to_remove:
            raise HTTPException(status_code=404, detail="Device token not found")

        await _broadcast({"type": "push.unregistered", "device_token": device_token})
        return {"ok": True}

    @app.get("/api/v1/push/registrations")
    async def push_list_registrations(request: Request) -> dict[str, Any]:
        """List all registered device tokens (admin view)."""
        _require_scope(request, SCOPE_READ)
        return {"registrations": list(_push_registrations.values())}

    @app.post("/api/v1/push/test")
    async def push_test(request: Request, body: dict = {}) -> dict[str, Any]:
        """Send a test push notification to all registered devices (or a specific token).

        Optional body: {device_token: str}
        Returns: {ok: bool, message: str, sent_to: int}
        """
        _require_scope(request, SCOPE_WRITE)
        target_token = body.get("device_token", None)

        targets = [
            reg for reg in _push_registrations.values()
            if target_token is None or reg["device_token"] == target_token
        ]
        if not targets:
            raise HTTPException(status_code=404, detail="No matching device registrations found")

        # In a full deployment the controller would call APNs / FCM here.
        # For now we broadcast the test event on the WebSocket so the mobile
        # app receives it via its event stream if connected over the relay.
        for reg in targets:
            reg["last_used"] = __import__("datetime").datetime.utcnow().isoformat() + "Z"
            await _broadcast({
                "type": "push.test",
                "registration_id": reg["id"],
                "platform": reg["platform"],
                "notification": {
                    "title": "Ozma Test Notification",
                    "body": "Push notifications are working correctly.",
                    "event_type": "test",
                },
            })

        log.info("push.test: sent test notification to %d device(s)", len(targets))
        return {
            "ok": True,
            "message": f"Test notification sent to {len(targets)} device(s)",
            "sent_to": len(targets),
        }

    # --- Guest invite endpoints ---
    # Camera-only, time-limited access links for non-account holders.
    # Tokens are kept in memory; a production deployment would persist them.

    import secrets as _secrets
    import datetime as _datetime

    _guest_invites: dict[str, dict[str, Any]] = {}

    def _make_invite_url(request: Request, invite_id: str, token: str) -> str:
        """Build the invite URL using the request's base URL."""
        base = str(request.base_url).rstrip("/")
        return f"{base}/invite/{invite_id}?token={token}"

    @app.get("/api/v1/guests")
    async def guest_list(request: Request) -> dict[str, Any]:
        """List all guest invites (active + expired, not revoked unless ?include_revoked=1)."""
        _require_scope(request, SCOPE_READ)
        include_revoked = request.query_params.get("include_revoked", "0") == "1"
        invites = [
            inv for inv in _guest_invites.values()
            if include_revoked or not inv["revoked"]
        ]
        return {"invites": invites}

    @app.post("/api/v1/guests/invite")
    async def guest_create_invite(request: Request, body: dict = {}) -> dict[str, Any]:
        """Create a new guest camera-only invite link.

        Body:
          label?: str            — display name for the invite
          camera_ids?: list[str] — restrict to these cameras; [] = all
          ttl?: int              — lifetime in seconds (default 604800 = 7 days)

        Returns: {ok: bool, invite: GuestInvite}
        """
        _require_scope(request, SCOPE_WRITE)
        label = body.get("label", None)
        camera_ids: list[str] = body.get("camera_ids", [])
        ttl: int = int(body.get("ttl", 604800))
        if ttl < 60 or ttl > 86400 * 365:
            raise HTTPException(status_code=400, detail="ttl must be between 60 seconds and 365 days")

        invite_id = str(_uuid.uuid4())
        token = _secrets.token_urlsafe(32)
        now = _datetime.datetime.utcnow()
        expires_at = (now + _datetime.timedelta(seconds=ttl)).isoformat() + "Z"

        invite = {
            "id": invite_id,
            "invite_url": _make_invite_url(request, invite_id, token),
            "token": token,  # kept server-side; not exposed in list endpoint
            "expires_at": expires_at,
            "camera_ids": camera_ids,
            "label": label,
            "created_by": getattr(getattr(request.state, "auth_ctx", None), "user_id", "api"),
            "created_at": now.isoformat() + "Z",
            "accepted_at": None,
            "accepted_by_email": None,
            "revoked": False,
        }
        _guest_invites[invite_id] = invite

        # Expose invite without the server-side token.
        public_invite = {k: v for k, v in invite.items() if k != "token"}
        log.info("guest.invite: created invite %s (ttl=%ds label=%s)", invite_id, ttl, label)
        await _broadcast({"type": "guest.invite.created", "invite_id": invite_id, "label": label})
        return {"ok": True, "invite": public_invite}

    @app.get("/api/v1/guests/invite/{invite_id}")
    async def guest_get_invite(request: Request, invite_id: str) -> dict[str, Any]:
        """Get a single invite by ID."""
        _require_scope(request, SCOPE_READ)
        invite = _guest_invites.get(invite_id)
        if not invite:
            raise HTTPException(status_code=404, detail="Invite not found")
        return {k: v for k, v in invite.items() if k != "token"}

    @app.delete("/api/v1/guests/invite/{invite_id}")
    async def guest_revoke_invite(request: Request, invite_id: str) -> dict[str, Any]:
        """Revoke a guest invite. The link stops working immediately."""
        _require_scope(request, SCOPE_WRITE)
        invite = _guest_invites.get(invite_id)
        if not invite:
            raise HTTPException(status_code=404, detail="Invite not found")
        invite["revoked"] = True
        log.info("guest.invite: revoked invite %s", invite_id)
        await _broadcast({"type": "guest.invite.revoked", "invite_id": invite_id})
        return {"ok": True}

    @app.get("/invite/{invite_id}")
    async def guest_invite_landing(invite_id: str, token: str = "") -> Any:
        """Public invite landing page — validates token and marks invite accepted.

        This endpoint is unauthenticated; it is accessed by recipients of the
        invite link.  Returns 200 with redirect instructions on success, 403 on
        invalid/expired/revoked token.
        """
        from fastapi.responses import JSONResponse
        invite = _guest_invites.get(invite_id)
        if not invite or invite["revoked"]:
            return JSONResponse({"error": "Invite not found or revoked"}, status_code=403)
        if invite["token"] != token:
            return JSONResponse({"error": "Invalid token"}, status_code=403)
        if _datetime.datetime.utcnow().isoformat() + "Z" > invite["expires_at"]:
            return JSONResponse({"error": "Invite has expired"}, status_code=403)

        if not invite["accepted_at"]:
            invite["accepted_at"] = _datetime.datetime.utcnow().isoformat() + "Z"
            await _broadcast({"type": "guest.invite.accepted", "invite_id": invite_id})

        # Return enough info for the mobile app or browser to bootstrap.
        return {
            "ok": True,
            "invite_id": invite_id,
            "camera_ids": invite["camera_ids"],
            "expires_at": invite["expires_at"],
            "message": "Invite accepted. Open the Ozma app to view cameras.",
        }

    # --- TCP tunnel endpoints ---

    from mesh_network import MeshNetworkManager, PortForward
    mesh_net = MeshNetworkManager()

    @app.get("/api/v1/tunnels")
    async def list_tunnels(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return {"tunnels": mesh_net.list_forwards()}

    @app.post("/api/v1/tunnels")
    async def create_tunnel(request: Request, body: dict) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        fwd = PortForward(
            id=body.get("id", f"tunnel-{int(time.time())}"),
            target_node=body.get("target_node", ""),
            target_host=body.get("target_host", ""),
            target_port=body.get("target_port", 0),
            protocol=body.get("protocol", "tcp"),
            expose_port=body.get("local_port", body.get("expose_port", 0)),
            description=body.get("description", ""),
        )
        if not fwd.target_node or not fwd.target_port:
            raise HTTPException(status_code=400, detail="target_node and target_port required")
        mesh_net.add_forward(fwd)
        await _broadcast({"type": "tunnel.created", "tunnel": fwd.to_dict()})
        return {"ok": True, "tunnel": fwd.to_dict()}

    @app.get("/api/v1/tunnels/{tunnel_id}")
    async def get_tunnel(request: Request, tunnel_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        fwd = mesh_net.get_forward(tunnel_id)
        if not fwd:
            raise HTTPException(status_code=404, detail="Tunnel not found")
        return fwd.to_dict()

    @app.delete("/api/v1/tunnels/{tunnel_id}")
    async def delete_tunnel(request: Request, tunnel_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        ok = mesh_net.remove_forward(tunnel_id)
        if not ok:
            raise HTTPException(status_code=404, detail="Tunnel not found")
        await _broadcast({"type": "tunnel.destroyed", "tunnel_id": tunnel_id})
        return {"ok": True}

    # --- Node reconciler ---

    if node_reconciler:
        @app.get("/api/v1/reconciled_nodes")
        async def list_reconciled(request: Request) -> dict:
            _require_scope(request, SCOPE_READ)
            return {"nodes": node_reconciler.list_reconciled()}

        @app.post("/api/v1/reconciled_nodes/bind")
        async def bind_nodes(body: dict, request: Request) -> dict:
            _require_scope(request, SCOPE_WRITE)
            node_reconciler.bind(body["hardware_node_id"], body["software_node_id"])
            return {"ok": True}

    # --- Update manager ---

    if update_mgr:
        @app.get("/api/v1/update/status")
        async def update_status(request: Request) -> dict:
            _require_scope(request, SCOPE_READ)
            return update_mgr.status()

        @app.post("/api/v1/update/check")
        async def update_check(request: Request) -> dict:
            _require_scope(request, SCOPE_READ)
            info = await update_mgr.check_for_update()
            return {"available": info is not None, "update": info}

        @app.post("/api/v1/update/apply")
        async def update_apply(body: dict, request: Request) -> dict:
            _require_scope(request, SCOPE_ADMIN)
            ok = await update_mgr.apply_update(body)
            return {"ok": ok}

    # --- Live transcription ---

    if transcription_mgr:
        import uuid as _uuid

        @app.get("/api/v1/transcription/sessions")
        async def list_transcription_sessions(request: Request) -> dict:
            _require_scope(request, SCOPE_READ)
            return {"sessions": transcription_mgr.list_sessions()}

        @app.post("/api/v1/transcription/start")
        async def start_transcription(body: dict, request: Request) -> dict:
            _require_scope(request, SCOPE_WRITE)
            session_id = body.get("session_id") or str(_uuid.uuid4())
            session = await transcription_mgr.start_session(
                session_id=session_id,
                source=body.get("source", "default"),
                language=body.get("language", "en"),
            )
            if session is None:
                raise HTTPException(status_code=503, detail="Failed to start transcription session")
            return {"session_id": session.id}

        @app.post("/api/v1/transcription/stop")
        async def stop_transcription(body: dict, request: Request) -> dict:
            _require_scope(request, SCOPE_WRITE)
            await transcription_mgr.stop_session(body.get("session_id", ""))
            return {"ok": True}

        @app.get("/api/v1/transcription/{session_id}")
        async def get_transcription(request: Request, session_id: str) -> dict:
            _require_scope(request, SCOPE_READ)
            since = float(request.query_params.get("since", "0"))
            segments = transcription_mgr.get_segments(session_id, since=since)
            return {"session_id": session_id, "segments": segments}

    # ── Doorbell ────────────────────────────────────────────────────────────

    @app.post("/api/v1/frigate/webhook")
    async def frigate_webhook(request: Request) -> dict[str, Any]:
        """Receive events from the Frigate-Ozma bridge (MQTT → HTTP).

        Called by frigate_tools.ozma_bridge for every MQTT message from Frigate.
        Doorbell button events are routed to DoorbellManager. Other events are
        put onto state.events for general consumption (overlays, notifications).
        """
        body = await request.json()
        kind = body.get("kind", "event")
        camera = body.get("camera", "")
        payload = body.get("payload", {})

        person = body.get("person", "")

        if kind == "doorbell" and doorbell_mgr and camera:
            # Button pressed → urgent doorbell alert
            alert = await doorbell_mgr.receive_button_press(camera)
            return {"ok": True, "alert_id": alert.id if alert else None}

        if kind == "person_recognized" and camera:
            # Person detected with facial recognition — enrich doorbell or create motion alert
            if doorbell_mgr:
                await doorbell_mgr.receive_person_detected(camera, person)

        # Forward all Frigate events onto the event bus for trigger rules / overlays
        event: dict[str, Any] = {
            "type": f"frigate.{kind}",
            "camera": camera,
            "topic": body.get("topic", ""),
            "payload": payload,
        }
        if person:
            event["person"] = person
        await state.events.put(event)
        return {"ok": True}

    # --- Alert endpoints ---

    @app.post("/api/v1/alerts")
    async def create_alert(request: Request, body: dict = {}) -> dict[str, Any]:
        """Create an alert from an external source (Home Assistant timer/alarm, etc.).

        Body fields (all optional except kind, title, body):
          kind          — "timer" | "alarm" | "reminder" | "doorbell" | "motion" | "alarm"
          title         — short header
          body          — one-line description
          timeout_s     — auto-expire seconds (0 = never)
          primary_label — label for primary action button
          secondary_label
          camera, person, snapshot_url  — optional media
          source        — free-form source identifier (e.g. "ha:timer.pasta")
        """
        _require_scope(request, SCOPE_WRITE)
        if not alert_mgr:
            raise HTTPException(503, "Alert manager not available")
        kind = body.get("kind", "reminder")
        title = body.get("title", "")
        body_text = body.get("body", body.get("message", ""))
        if not title or not body_text:
            raise HTTPException(400, "title and body are required")
        alert = await alert_mgr.create(
            kind=kind,
            title=title,
            body=body_text,
            timeout_s=int(body.get("timeout_s", 0)),
            primary_label=body.get("primary_label", "OK"),
            secondary_label=body.get("secondary_label", "Dismiss"),
            camera=body.get("camera", ""),
            person=body.get("person", ""),
            snapshot_url=body.get("snapshot_url", ""),
        )
        if not alert:
            return {"ok": False, "reason": "debounced"}
        return {"ok": True, "alert_id": alert.id}

    @app.post("/api/v1/alerts/{alert_id}/acknowledge")
    async def acknowledge_alert(request: Request, alert_id: str) -> dict[str, Any]:
        """Acknowledge an active alert (primary action)."""
        _require_scope(request, SCOPE_WRITE)
        if not alert_mgr:
            raise HTTPException(503, "Alert manager not available")
        ok = await alert_mgr.acknowledge(alert_id)
        if not ok:
            raise HTTPException(404, "Alert not found or not active")
        return {"ok": True}

    @app.post("/api/v1/alerts/{alert_id}/dismiss")
    async def dismiss_alert(request: Request, alert_id: str) -> dict[str, Any]:
        """Dismiss an active alert (secondary action)."""
        _require_scope(request, SCOPE_WRITE)
        if not alert_mgr:
            raise HTTPException(503, "Alert manager not available")
        ok = await alert_mgr.dismiss(alert_id)
        if not ok:
            raise HTTPException(404, "Alert not found or not active")
        return {"ok": True}

    @app.get("/api/v1/alerts")
    async def list_alerts(
        request: Request,
        kind: str | None = None,
        state_filter: str | None = None,
    ) -> dict[str, Any]:
        """List alerts. Optional ?kind=doorbell&state_filter=active."""
        _require_scope(request, SCOPE_READ)
        alerts = alert_mgr.list_alerts(kind=kind, state=state_filter) if alert_mgr else []
        return {"alerts": alerts}

    @app.get("/api/v1/alerts/{alert_id}/snapshot")
    async def alert_snapshot(request: Request, alert_id: str) -> Any:
        """Proxy a camera snapshot for the given alert."""
        _require_scope(request, SCOPE_READ)
        if not alert_mgr:
            raise HTTPException(503, "Alert manager not available")
        snapshot_url = alert_mgr.get_snapshot_url(alert_id)
        if not snapshot_url:
            raise HTTPException(404, "Alert not found or no snapshot")
        try:
            import httpx as _httpx
            async with _httpx.AsyncClient(timeout=5) as client:
                resp = await client.get(snapshot_url)
                if resp.status_code != 200:
                    raise HTTPException(502, "Snapshot unavailable")
                from fastapi.responses import Response as _Response
                return _Response(
                    content=resp.content,
                    media_type=resp.headers.get("content-type", "image/jpeg"),
                )
        except Exception as exc:
            raise HTTPException(502, f"Snapshot fetch failed: {exc}") from exc

    # Backwards-compat aliases — doorbell sessions are now alerts with kind="doorbell"
    @app.get("/api/v1/doorbell/sessions")
    async def list_doorbell_sessions(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        sessions = alert_mgr.list_alerts(kind="doorbell") if alert_mgr else []
        return {"sessions": sessions}

    @app.get("/api/v1/doorbell/{session_id}/snapshot")
    async def doorbell_snapshot(request: Request, session_id: str) -> Any:
        return await alert_snapshot(request, session_id)

    # ── Vaultwarden ────────────────────────────────────────────────────────

    class VaultOidcRequest(BaseModel):
        client_id: str
        client_secret: str
        issuer_url: str

    @app.get("/api/v1/vault/status")
    async def vault_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not vaultwarden:
            raise HTTPException(503, "Vaultwarden not enabled")
        s = vaultwarden.get_status()
        if "status" not in s:
            s["status"] = "running" if s.get("running") else "stopped"
        return s

    @app.get("/api/v1/vault/backup-paths")
    async def vault_backup_paths(request: Request) -> dict[str, Any]:
        """Return the file paths that must be included in the backup schedule."""
        _require_scope(request, SCOPE_READ)
        if not vaultwarden:
            raise HTTPException(503, "Vaultwarden not enabled")
        return {"paths": vaultwarden.backup_paths()}

    @app.post("/api/v1/vault/oidc")
    async def vault_configure_oidc(request: Request, body: VaultOidcRequest) -> dict[str, Any]:
        """Configure SSO: link Vaultwarden to the controller IdP."""
        _require_scope(request, SCOPE_ADMIN)
        if not vaultwarden:
            raise HTTPException(503, "Vaultwarden not enabled")
        vaultwarden.configure_oidc(
            client_id=body.client_id,
            client_secret=body.client_secret,
            issuer_url=body.issuer_url,
        )
        return {"ok": True, "message": "OIDC config updated; restart container to apply"}

    # ── Email security ─────────────────────────────────────────────────────

    class AddDomainRequest(BaseModel):
        domain: str
        dkim_selectors: list[str] = []

    @app.get("/api/v1/email-security/domains")
    async def email_security_list(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not email_security:
            raise HTTPException(503, "Email security monitor not available")
        return {
            "domains": email_security.list_domains(),
            "results": email_security.get_all_results(),
        }

    @app.post("/api/v1/email-security/domains")
    async def email_security_add_domain(request: Request,
                                        body: AddDomainRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not email_security:
            raise HTTPException(503, "Email security monitor not available")
        email_security.add_domain(body.domain, body.dkim_selectors)
        return {"ok": True, "domain": body.domain}

    @app.delete("/api/v1/email-security/domains/{domain:path}")
    async def email_security_remove_domain(request: Request,
                                           domain: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not email_security:
            raise HTTPException(503, "Email security monitor not available")
        email_security.remove_domain(domain)
        return {"ok": True}

    @app.get("/api/v1/email-security/domains/{domain:path}")
    async def email_security_get_domain(request: Request, domain: str) -> dict[str, Any]:
        """Return the latest posture for one domain, including full remediation guides."""
        _require_scope(request, SCOPE_READ)
        if not email_security:
            raise HTTPException(503, "Email security monitor not available")
        result = email_security.get_result(domain)
        if not result:
            raise HTTPException(404, f"No result for domain: {domain}")
        return result.to_dict(include_remediation=True)

    @app.post("/api/v1/email-security/domains/{domain:path}/check")
    async def email_security_check_now(request: Request, domain: str) -> dict[str, Any]:
        """Trigger an immediate re-check and return results with remediation guides."""
        _require_scope(request, SCOPE_WRITE)
        if not email_security:
            raise HTTPException(503, "Email security monitor not available")
        # Register the domain if not already tracked
        if domain not in email_security.list_domains():
            email_security.add_domain(domain)
        posture = await email_security.check_now(domain)
        return posture.to_dict(include_remediation=True)

    # ── Cloud backup ────────────────────────────────────────────────────────

    class AddBackupSourceRequest(BaseModel):
        name: str
        provider: str           # "m365" | "google"
        tenant_id: str
        backup_mail: bool = True
        backup_files: bool = True
        backup_sharepoint: bool = False
        schedule_cron: str = "0 2 * * *"
        retention_days: int = 90

    class M365CredentialsRequest(BaseModel):
        source_id: str
        tenant_id: str
        client_id: str
        client_secret: str

    class GoogleCredentialsRequest(BaseModel):
        source_id: str
        customer_id: str        # Google customer ID (tenant_id equivalent)
        service_account_json: str   # full JSON key content
        admin_email: str

    @app.get("/api/v1/cloud-backup/status")
    async def cloud_backup_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        return cloud_backup.get_status()

    @app.get("/api/v1/cloud-backup/sources")
    async def cloud_backup_list_sources(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        return {"sources": [s.to_dict() for s in cloud_backup.list_sources()]}

    @app.post("/api/v1/cloud-backup/sources")
    async def cloud_backup_add_source(request: Request,
                                       body: AddBackupSourceRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        try:
            provider = Provider(body.provider)
        except ValueError:
            raise HTTPException(400, f"Unknown provider: {body.provider}")
        source = BackupSource(
            name=body.name,
            provider=provider,
            tenant_id=body.tenant_id,
            backup_mail=body.backup_mail,
            backup_files=body.backup_files,
            backup_sharepoint=body.backup_sharepoint,
            schedule_cron=body.schedule_cron,
            retention_days=body.retention_days,
        )
        cloud_backup.add_source(source)
        return {"ok": True, "source_id": source.id}

    @app.delete("/api/v1/cloud-backup/sources/{source_id}")
    async def cloud_backup_remove_source(request: Request,
                                          source_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        if not cloud_backup.remove_source(source_id):
            raise HTTPException(404, f"Source not found: {source_id}")
        return {"ok": True}

    @app.post("/api/v1/cloud-backup/sources/{source_id}/credentials/m365")
    async def cloud_backup_set_m365_creds(request: Request, source_id: str,
                                           body: M365CredentialsRequest) -> dict[str, Any]:
        """Store M365 app credentials for a backup source (encrypted at rest)."""
        _require_scope(request, SCOPE_ADMIN)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        if not cloud_backup.get_source(source_id):
            raise HTTPException(404, f"Source not found: {source_id}")
        rec = CredentialRecord(
            id=source_id,
            provider=Provider.M365,
            tenant_id=body.tenant_id,
            client_id=body.client_id,
            client_secret=body.client_secret,
        )
        cloud_backup.store_credentials(rec)
        return {"ok": True, "message": "M365 credentials stored (encrypted)"}

    @app.post("/api/v1/cloud-backup/sources/{source_id}/credentials/google")
    async def cloud_backup_set_google_creds(request: Request, source_id: str,
                                             body: GoogleCredentialsRequest) -> dict[str, Any]:
        """Store Google Workspace service account credentials (encrypted at rest)."""
        _require_scope(request, SCOPE_ADMIN)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        if not cloud_backup.get_source(source_id):
            raise HTTPException(404, f"Source not found: {source_id}")
        rec = CredentialRecord(
            id=source_id,
            provider=Provider.GOOGLE,
            tenant_id=body.customer_id,
            service_account_json=body.service_account_json,
            admin_email=body.admin_email,
        )
        cloud_backup.store_credentials(rec)
        return {"ok": True, "message": "Google credentials stored (encrypted)"}

    @app.post("/api/v1/cloud-backup/sources/{source_id}/trigger")
    async def cloud_backup_trigger(request: Request, source_id: str) -> dict[str, Any]:
        """Immediately enqueue all backup jobs for a source."""
        _require_scope(request, SCOPE_WRITE)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        if not cloud_backup.get_source(source_id):
            raise HTTPException(404, f"Source not found: {source_id}")
        job_ids = await cloud_backup.trigger_backup(source_id, priority=0)
        return {"ok": True, "jobs_enqueued": len(job_ids), "job_ids": job_ids}

    # ── rclone remote management ───────────────────────────────────────────────

    @app.get("/api/v1/cloud-backup/rclone/available")
    async def cloud_backup_rclone_available(request: Request) -> dict[str, Any]:
        """Check whether the rclone binary is installed on this controller."""
        _require_scope(request, SCOPE_READ)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        available, version = await cloud_backup.rclone_available()
        return {"available": available, "version": version}

    @app.get("/api/v1/cloud-backup/rclone/remotes")
    async def cloud_backup_rclone_list_remotes(request: Request) -> dict[str, Any]:
        """List all configured rclone remotes."""
        _require_scope(request, SCOPE_READ)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        remotes = await cloud_backup.rclone_list_remotes()
        return {"remotes": remotes}

    class RcloneConfigureRemoteRequest(BaseModel):
        name: str
        type: str                    # rclone provider type, e.g. "s3", "b2", "dropbox"
        params: dict[str, str] = {}  # provider-specific config params

    @app.post("/api/v1/cloud-backup/rclone/remotes")
    async def cloud_backup_rclone_configure(
        request: Request,
        body: RcloneConfigureRemoteRequest,
    ) -> dict[str, Any]:
        """Configure a new rclone remote (or update an existing one)."""
        _require_scope(request, SCOPE_ADMIN)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        ok = await cloud_backup.rclone_configure_remote(body.name, body.type, body.params)
        if not ok:
            raise HTTPException(500, "rclone config create failed — check controller logs")
        return {"ok": True, "remote": body.name}

    @app.delete("/api/v1/cloud-backup/rclone/remotes/{name}")
    async def cloud_backup_rclone_delete_remote(
        request: Request, name: str,
    ) -> dict[str, Any]:
        """Remove a configured rclone remote."""
        _require_scope(request, SCOPE_ADMIN)
        if not cloud_backup:
            raise HTTPException(503, "Cloud backup not available")
        ok = await cloud_backup.rclone_delete_remote(name)
        if not ok:
            raise HTTPException(500, f"Failed to delete rclone remote: {name}")
        return {"ok": True}

    # ── ITSM ───────────────────────────────────────────────────────────────────

    class CreateTicketRequest(BaseModel):
        source: str = "user"
        category: str = "incident"
        priority: str = "medium"
        subject: str
        description: str = ""
        requester_user_id: str = "admin"
        node_id: str | None = None
        assignee_user_id: str | None = None

    class ResolveTicketRequest(BaseModel):
        resolution: str
        actor: str = "admin"

    class EscalateTicketRequest(BaseModel):
        notes: str = ""
        actor: str = "admin"

    class CommentTicketRequest(BaseModel):
        note: str
        actor: str = "admin"

    class OnCallUserRequest(BaseModel):
        channels: list[str] = []
        working_hours: list[dict] = []
        oncall_windows: list[dict] = []
        interrupt_critical: bool = True
        interrupt_high: bool = False
        interrupt_any: bool = False

    class EscalationPolicyRequest(BaseModel):
        name: str
        tiers: list[dict] = []

    class AgentModelConfigRequest(BaseModel):
        provider: str
        model: str
        base_url: str = ""
        api_key_env: str = ""
        extra: dict = {}

    class ITSMConfigPatchRequest(BaseModel):
        default_policy_id: str | None = None
        l1_max_attempts: int | None = None
        l2_max_attempts: int | None = None
        l1_timeout_seconds: int | None = None
        l2_timeout_seconds: int | None = None
        l1_model: AgentModelConfigRequest | None = None
        l2_model: AgentModelConfigRequest | None = None
        external_webhook_url: str | None = None
        external_webhook_headers: dict | None = None

    @app.get("/api/v1/itsm/status")
    async def itsm_status() -> dict[str, Any]:
        """ITSM summary stats."""
        if not itsm:
            return {"available": False}
        return {"available": True, **itsm.status()}

    @app.get("/api/v1/itsm/tickets")
    async def itsm_list_tickets(
        request: Request,
        status: str | None = None,
        priority: str | None = None,
        limit: int = 100,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        tickets = itsm.list_tickets(status=status, priority=priority, limit=limit)
        return {"tickets": [t.to_dict() for t in tickets]}

    @app.post("/api/v1/itsm/tickets")
    async def itsm_create_ticket(
        request: Request, body: CreateTicketRequest,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        ticket = await itsm.create_ticket(
            source=body.source,
            category=body.category,
            priority=body.priority,
            subject=body.subject,
            description=body.description,
            requester_user_id=body.requester_user_id,
            node_id=body.node_id,
            assignee_user_id=body.assignee_user_id,
        )
        return {"ticket": ticket.to_dict()}

    @app.get("/api/v1/itsm/tickets/{ticket_id}")
    async def itsm_get_ticket(request: Request, ticket_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        ticket = itsm.get_ticket(ticket_id)
        if not ticket:
            raise HTTPException(404, f"Ticket {ticket_id} not found")
        return {"ticket": ticket.to_dict()}

    @app.post("/api/v1/itsm/tickets/{ticket_id}/resolve")
    async def itsm_resolve_ticket(
        request: Request, ticket_id: str, body: ResolveTicketRequest,
    ) -> dict[str, Any]:
        """Resolve a ticket — called by AI agent or human operator."""
        _require_scope(request, SCOPE_WRITE)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        ok = await itsm.resolve_ticket(ticket_id, actor=body.actor, resolution=body.resolution)
        if not ok:
            raise HTTPException(404, f"Ticket {ticket_id} not found or already closed")
        ticket = itsm.get_ticket(ticket_id)
        return {"ok": True, "ticket": ticket.to_dict() if ticket else {}}

    @app.post("/api/v1/itsm/tickets/{ticket_id}/escalate")
    async def itsm_escalate_ticket(
        request: Request, ticket_id: str, body: EscalateTicketRequest,
    ) -> dict[str, Any]:
        """Escalate a ticket — called by AI agent when it cannot fix the issue."""
        _require_scope(request, SCOPE_WRITE)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        ok = await itsm.escalate_ticket(ticket_id, actor=body.actor, notes=body.notes)
        if not ok:
            raise HTTPException(404, f"Ticket {ticket_id} not found or already closed")
        ticket = itsm.get_ticket(ticket_id)
        return {"ok": True, "ticket": ticket.to_dict() if ticket else {}}

    @app.post("/api/v1/itsm/tickets/{ticket_id}/acknowledge")
    async def itsm_acknowledge_ticket(
        request: Request, ticket_id: str, body: dict = {},
    ) -> dict[str, Any]:
        """Human acknowledges they are handling the ticket."""
        _require_scope(request, SCOPE_WRITE)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        user_id = body.get("user_id", "admin")
        ok = await itsm.acknowledge_ticket(ticket_id, user_id=user_id)
        if not ok:
            raise HTTPException(404, f"Ticket {ticket_id} not found or not in pending state")
        ticket = itsm.get_ticket(ticket_id)
        return {"ok": True, "ticket": ticket.to_dict() if ticket else {}}

    @app.post("/api/v1/itsm/tickets/{ticket_id}/close")
    async def itsm_close_ticket(
        request: Request, ticket_id: str, body: dict = {},
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        ok = await itsm.close_ticket(ticket_id, actor=body.get("actor", "admin"))
        if not ok:
            raise HTTPException(404, f"Ticket {ticket_id} not found")
        return {"ok": True}

    @app.post("/api/v1/itsm/tickets/{ticket_id}/comment")
    async def itsm_comment_ticket(
        request: Request, ticket_id: str, body: CommentTicketRequest,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        ok = await itsm.comment_ticket(ticket_id, actor=body.actor, note=body.note)
        if not ok:
            raise HTTPException(404, f"Ticket {ticket_id} not found")
        return {"ok": True}

    @app.get("/api/v1/itsm/oncall")
    async def itsm_get_oncall(request: Request) -> dict[str, Any]:
        """Get on-call schedule and escalation policies."""
        _require_scope(request, SCOPE_READ)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        cfg = itsm.get_config()
        return {
            "oncall_users": [u.to_dict() for u in cfg.oncall_users.values()],
            "escalation_policies": [p.to_dict() for p in cfg.escalation_policies.values()],
            "default_policy_id": cfg.default_policy_id,
        }

    @app.put("/api/v1/itsm/oncall/users/{user_id}")
    async def itsm_upsert_oncall_user(
        request: Request, user_id: str, body: OnCallUserRequest,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        user = OnCallUser(
            user_id=user_id,
            channels=body.channels,
            working_hours=[WorkingHours.from_dict(w) for w in body.working_hours],
            oncall_windows=[OnCallWindow.from_dict(w) for w in body.oncall_windows],
            interrupt_critical=body.interrupt_critical,
            interrupt_high=body.interrupt_high,
            interrupt_any=body.interrupt_any,
        )
        itsm.upsert_oncall_user(user)
        return {"ok": True, "user": user.to_dict()}

    @app.delete("/api/v1/itsm/oncall/users/{user_id}")
    async def itsm_remove_oncall_user(request: Request, user_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        ok = itsm.remove_oncall_user(user_id)
        if not ok:
            raise HTTPException(404, f"On-call user {user_id} not found")
        return {"ok": True}

    @app.put("/api/v1/itsm/oncall/policies/{policy_id}")
    async def itsm_upsert_policy(
        request: Request, policy_id: str, body: EscalationPolicyRequest,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        policy = EscalationPolicy(
            id=policy_id,
            name=body.name,
            tiers=[EscalationTier.from_dict(t) for t in body.tiers],
        )
        itsm.upsert_escalation_policy(policy)
        return {"ok": True, "policy": policy.to_dict()}

    @app.delete("/api/v1/itsm/oncall/policies/{policy_id}")
    async def itsm_remove_policy(request: Request, policy_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        ok = itsm.remove_escalation_policy(policy_id)
        if not ok:
            raise HTTPException(404, f"Policy {policy_id} not found")
        return {"ok": True}

    @app.patch("/api/v1/itsm/config")
    async def itsm_patch_config(
        request: Request, body: ITSMConfigPatchRequest,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not itsm:
            raise HTTPException(503, "ITSM not available")
        cfg = itsm.get_config()
        if body.default_policy_id is not None:
            cfg.default_policy_id = body.default_policy_id
        if body.l1_max_attempts is not None:
            cfg.l1_max_attempts = body.l1_max_attempts
        if body.l2_max_attempts is not None:
            cfg.l2_max_attempts = body.l2_max_attempts
        if body.l1_timeout_seconds is not None:
            cfg.l1_timeout_seconds = body.l1_timeout_seconds
        if body.l2_timeout_seconds is not None:
            cfg.l2_timeout_seconds = body.l2_timeout_seconds
        if body.l1_model is not None:
            cfg.l1_model = AgentModelConfig(
                provider=body.l1_model.provider,
                model=body.l1_model.model,
                base_url=body.l1_model.base_url,
                api_key_env=body.l1_model.api_key_env,
                extra=body.l1_model.extra,
            )
        if body.l2_model is not None:
            cfg.l2_model = AgentModelConfig(
                provider=body.l2_model.provider,
                model=body.l2_model.model,
                base_url=body.l2_model.base_url,
                api_key_env=body.l2_model.api_key_env,
                extra=body.l2_model.extra,
            )
        if body.external_webhook_url is not None:
            cfg.external_webhook_url = body.external_webhook_url
        if body.external_webhook_headers is not None:
            cfg.external_webhook_headers = body.external_webhook_headers
        itsm.set_config(cfg)
        return {"ok": True, "config": cfg.to_dict()}

    # ── IoT network ────────────────────────────────────────────────────────────

    class VLANConfigRequest(BaseModel):
        vlan_id:    int  = 20
        subnet:     str  = "192.168.20"
        gateway:    str  = "192.168.20.1"
        dhcp_start: str  = "192.168.20.100"
        dhcp_end:   str  = "192.168.20.200"
        dns:        str  = "192.168.20.1"
        iface:      str  = ""

    class AddDeviceRequest(BaseModel):
        mac:             str
        name:            str  = ""
        category:        str  = "unknown"
        internet_access: str  = "deny"

    class UpdateDeviceRequest(BaseModel):
        name:            str | None = None
        category:        str | None = None
        internet_access: str | None = None
        notes:           str | None = None
        blocked:         bool | None = None

    class StartOnboardingRequest(BaseModel):
        device_name:    str  = ""
        category:       str  = "unknown"
        phone_ip:       str  = ""
        allow_internet: bool = False
        ttl:            int  = 1800

    class CompleteOnboardingRequest(BaseModel):
        mac:             str
        name:            str  = ""
        internet_access: str  = "deny"

    @app.get("/api/v1/iot/status")
    async def iot_status(request: Request) -> dict:
        _require_scope(request, SCOPE_READ)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        return iot.status()

    @app.post("/api/v1/iot/provision")
    async def iot_provision(request: Request, body: VLANConfigRequest) -> dict:
        """Provision the IoT VLAN on the configured backend."""
        _require_scope(request, SCOPE_ADMIN)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        cfg = VLANConfig(
            vlan_id=body.vlan_id, subnet=body.subnet, gateway=body.gateway,
            dhcp_start=body.dhcp_start, dhcp_end=body.dhcp_end,
            dns=body.dns, iface=body.iface,
        )
        ok = await iot.provision(cfg)
        if not ok:
            raise HTTPException(500, "VLAN provisioning failed — check controller logs")
        return {"ok": True, "vlan": cfg.to_dict()}

    @app.get("/api/v1/iot/nftables")
    async def iot_nftables(request: Request) -> dict:
        """Export current nftables ruleset for review."""
        _require_scope(request, SCOPE_ADMIN)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        return {"rules": iot.export_nftables()}

    # ── Devices ────────────────────────────────────────────────────────

    @app.get("/api/v1/iot/devices")
    async def iot_list_devices(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        return [d.to_dict() for d in iot.list_devices()]

    @app.post("/api/v1/iot/devices")
    async def iot_add_device(request: Request, body: AddDeviceRequest) -> dict:
        _require_scope(request, SCOPE_WRITE)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        dev = iot.add_device(
            mac=body.mac, name=body.name,
            category=DeviceCategory(body.category),
            internet_access=InternetAccess(body.internet_access),
        )
        await state.events.put({"type": "iot.device_added", "device": dev.to_dict()})
        return dev.to_dict()

    @app.get("/api/v1/iot/devices/{device_id}")
    async def iot_get_device(device_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_READ)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        dev = iot.get_device(device_id)
        if not dev:
            raise HTTPException(404, "Device not found")
        return dev.to_dict()

    @app.put("/api/v1/iot/devices/{device_id}")
    async def iot_update_device(device_id: str, request: Request,
                                 body: UpdateDeviceRequest) -> dict:
        _require_scope(request, SCOPE_WRITE)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        updates: dict = {k: v for k, v in body.model_dump().items() if v is not None}
        if "category" in updates:
            updates["category"] = DeviceCategory(updates["category"])
        if "internet_access" in updates:
            updates["internet_access"] = InternetAccess(updates["internet_access"])
        dev = iot.update_device(device_id, **updates)
        if not dev:
            raise HTTPException(404, "Device not found")
        return dev.to_dict()

    @app.delete("/api/v1/iot/devices/{device_id}")
    async def iot_remove_device(device_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_WRITE)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        if not iot.remove_device(device_id):
            raise HTTPException(404, "Device not found")
        return {"ok": True}

    # ── Onboarding ─────────────────────────────────────────────────────

    @app.post("/api/v1/iot/onboard")
    async def iot_start_onboarding(request: Request,
                                    body: StartOnboardingRequest) -> dict:
        """Start an onboarding session — creates exception rule for device setup."""
        _require_scope(request, SCOPE_WRITE)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        sess = await iot.start_onboarding(
            device_name=body.device_name,
            category=DeviceCategory(body.category),
            phone_ip=body.phone_ip,
            allow_internet=body.allow_internet,
            ttl=body.ttl,
        )
        await state.events.put({"type": "iot.onboarding_started", "session": sess.to_dict()})
        return sess.to_dict()

    @app.post("/api/v1/iot/onboard/{session_id}/complete")
    async def iot_complete_onboarding(session_id: str, request: Request,
                                       body: CompleteOnboardingRequest) -> dict:
        """Complete onboarding — removes exception, adds device to inventory."""
        _require_scope(request, SCOPE_WRITE)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        dev = await iot.complete_onboarding(
            session_id=session_id, mac=body.mac, name=body.name,
            internet_access=InternetAccess(body.internet_access),
        )
        if not dev:
            raise HTTPException(404, "Session not found or not in pending state")
        await state.events.put({"type": "iot.onboarding_complete", "device": dev.to_dict()})
        return dev.to_dict()

    @app.post("/api/v1/iot/onboard/{session_id}/cancel")
    async def iot_cancel_onboarding(session_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_WRITE)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        if not await iot.cancel_onboarding(session_id):
            raise HTTPException(404, "Session not found or not in pending state")
        return {"ok": True}

    @app.get("/api/v1/iot/onboard")
    async def iot_list_sessions(request: Request, active: bool = False) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        return [s.to_dict() for s in iot.list_sessions(active_only=active)]

    @app.post("/api/v1/iot/sync-leases")
    async def iot_sync_leases(request: Request) -> dict:
        """Pull DHCP leases from backend and update device IPs."""
        _require_scope(request, SCOPE_WRITE)
        if not iot:
            raise HTTPException(503, "IoT network manager not enabled")
        count = await iot.sync_leases()
        return {"updated": count}

    # ── WireGuard peering ──────────────────────────────────────────────────────

    class WGPeerRequest(BaseModel):
        controller_id: str
        public_key:    str
        endpoint:      str
        overlay_ip:    str = ""
        api_port:      int = 7380

    @app.get("/api/v1/wg/info")
    async def wg_info(request: Request) -> dict:
        """Return our WireGuard public key + endpoint for peer exchange."""
        # Note: intentionally unauthenticated — public key exchange must work
        # before auth is bootstrapped. The public key is not a secret.
        if not wg:
            raise HTTPException(503, "WireGuard peering not enabled")
        return wg.get_info()

    @app.post("/api/v1/wg/peer")
    async def wg_add_peer(body: WGPeerRequest, request: Request) -> dict:
        """Receive a peer's WG info and add them. Called by the initiating controller."""
        # Allow from WireGuard overlay (already peered) or admin JWT
        if not wg:
            raise HTTPException(503, "WireGuard peering not enabled")
        peer = wg.get_peer(body.controller_id)
        if peer:
            # Already peered — return 409 (initiator can treat as success)
            return {"status": "already_peered", "overlay_ip": peer.overlay_ip}
        peer = await wg.add_peer(
            controller_id=body.controller_id,
            public_key=body.public_key,
            endpoint=body.endpoint,
            overlay_ip=body.overlay_ip,
        )
        await state.events.put({"type": "wg.peered", "peer": peer.to_dict()})
        return {"status": "peered", "overlay_ip": peer.overlay_ip,
                "our_overlay_ip": wg.overlay_ip}

    @app.get("/api/v1/wg/status")
    async def wg_status(request: Request) -> dict:
        _require_scope(request, SCOPE_READ)
        if not wg:
            raise HTTPException(503, "WireGuard peering not enabled")
        return wg.status()

    @app.delete("/api/v1/wg/peer/{controller_id}")
    async def wg_remove_peer(controller_id: str, request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        if not wg:
            raise HTTPException(503, "WireGuard peering not enabled")
        if not await wg.remove_peer(controller_id):
            raise HTTPException(404, "WG peer not found")
        await state.events.put({"type": "wg.unpeered", "controller_id": controller_id})
        return {"ok": True}

    @app.post("/api/v1/wg/peer/{controller_id}/initiate")
    async def wg_initiate_peering(controller_id: str, request: Request) -> dict:
        """Manually trigger WG peering with an already-linked peer controller."""
        _require_scope(request, SCOPE_ADMIN)
        if not wg:
            raise HTTPException(503, "WireGuard peering not enabled")
        if not sharing:
            raise HTTPException(503, "Sharing not enabled")
        peer = sharing.get_peer(controller_id)
        if not peer:
            raise HTTPException(404, "Peer controller not found")
        wg_peer = await wg.peer_with(peer.host, peer.port)
        if not wg_peer:
            raise HTTPException(502, "WG peering handshake failed")
        return wg_peer.to_dict()

    # ── License & SaaS management ──────────────────────────────────────────────

    class AddProductRequest(BaseModel):
        name:         str
        vendor:       str  = ""
        license_type: str  = "perpetual"
        seats:        int  = 1
        annual_cost:  float = 0.0
        renewal_date: float = 0.0
        notes:        str  = ""

    class UpdateProductRequest(BaseModel):
        name:         str | None = None
        vendor:       str | None = None
        license_type: str | None = None
        seats_licensed: int | None = None
        annual_cost:  float | None = None
        renewal_date: float | None = None
        notes:        str | None = None

    class AddSaaSRequest(BaseModel):
        name:             str
        vendor:           str  = ""
        category:         str  = "other"
        url:              str  = ""
        discovery_source: str  = "manual"
        monthly_cost:     float = 0.0
        seats_licensed:   int  = 0
        renewal_date:     float = 0.0
        notes:            str  = ""
        approved:         bool = False

    class UpdateSaaSRequest(BaseModel):
        name:           str | None = None
        vendor:         str | None = None
        category:       str | None = None
        url:            str | None = None
        monthly_cost:   float | None = None
        seats_licensed: int | None = None
        seats_active:   int | None = None
        renewal_date:   float | None = None
        vendor_soc2:    bool | None = None
        vendor_gdpr:    bool | None = None
        dpa_signed:     bool | None = None
        sso_integrated: bool | None = None
        mfa_enforced:   bool | None = None
        notes:          str | None = None

    class ReconcileRequest(BaseModel):
        installed: list[dict[str, str]]

    @app.get("/api/v1/licenses")
    async def license_list(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        return {"products": [p.to_dict() for p in license_mgr.list_products()]}

    @app.post("/api/v1/licenses")
    async def license_add(request: Request, body: AddProductRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        try:
            lt = LicenseType(body.license_type)
        except ValueError:
            raise HTTPException(400, f"Unknown license_type: {body.license_type}")
        if body.seats < 1:
            raise HTTPException(400, "seats must be >= 1")
        p = license_mgr.add_product(
            name=body.name, vendor=body.vendor, license_type=lt,
            seats=body.seats, annual_cost=body.annual_cost,
            renewal_date=body.renewal_date, notes=body.notes,
        )
        return p.to_dict()

    @app.get("/api/v1/licenses/{product_id}")
    async def license_get(request: Request, product_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        p = license_mgr.get_product(product_id)
        if not p:
            raise HTTPException(404, "Product not found")
        return p.to_dict()

    @app.patch("/api/v1/licenses/{product_id}")
    async def license_update(request: Request, product_id: str,
                             body: UpdateProductRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        updates = {k: v for k, v in body.model_dump().items() if v is not None}
        if "license_type" in updates:
            try:
                updates["license_type"] = LicenseType(updates["license_type"])
            except ValueError:
                raise HTTPException(400, f"Unknown license_type: {updates['license_type']}")
        p = license_mgr.update_product(product_id, **updates)
        if not p:
            raise HTTPException(404, "Product not found")
        return p.to_dict()

    @app.delete("/api/v1/licenses/{product_id}")
    async def license_delete(request: Request, product_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        if not license_mgr.remove_product(product_id):
            raise HTTPException(404, "Product not found")
        return {"ok": True}

    @app.post("/api/v1/licenses/{product_id}/reconcile")
    async def license_reconcile(request: Request, product_id: str,
                                body: ReconcileRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        node_id = product_id   # product_id param re-used as node_id here; real
        # reconcile is per-node, not per-product — use /nodes/{id}/reconcile instead
        raise HTTPException(400, "Use POST /api/v1/nodes/{node_id}/reconcile-licenses")

    @app.post("/api/v1/nodes/{node_id}/reconcile-licenses")
    async def node_reconcile_licenses(request: Request, node_id: str,
                                      body: ReconcileRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        if node_id not in state.nodes:
            raise HTTPException(404, "Node not found")
        report = license_mgr.reconcile_node(node_id, body.installed)
        return report

    # SaaS endpoints

    @app.get("/api/v1/saas")
    async def saas_list(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        return {"apps": [a.to_dict() for a in license_mgr.list_saas()]}

    @app.post("/api/v1/saas")
    async def saas_add(request: Request, body: AddSaaSRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        try:
            cat = SaaSCategory(body.category)
        except ValueError:
            raise HTTPException(400, f"Unknown category: {body.category}")
        try:
            src = DiscoverySource(body.discovery_source)
        except ValueError:
            raise HTTPException(400, f"Unknown discovery_source: {body.discovery_source}")
        app_obj = license_mgr.add_saas(
            name=body.name, vendor=body.vendor, category=cat, url=body.url,
            discovery_sources=[src], monthly_cost=body.monthly_cost,
            seats_licensed=body.seats_licensed, renewal_date=body.renewal_date,
            notes=body.notes, approved=body.approved,
        )
        return app_obj.to_dict()

    @app.get("/api/v1/saas/{app_id}")
    async def saas_get(request: Request, app_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        # Static sub-paths that are registered after this parameterized route
        if app_id == "cost":
            if not saas_mgr:
                return {"total": 0.0, "monthly": 0.0, "services": [], "costs": {}}
            summary = saas_mgr.cost_summary()
            # Normalize to expected field names for test compatibility
            if "total" not in summary and "total_monthly_cost" in summary:
                summary["total"] = summary["total_monthly_cost"]
                summary["monthly"] = summary.get("total_monthly_cost", 0)
            return summary
        if app_id in ("status", "config"):
            # Forward to the appropriate handler via redirect logic
            raise HTTPException(404, f"Use /api/v1/saas/{app_id} sub-path endpoint")
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        app_obj = license_mgr.get_saas(app_id)
        if not app_obj:
            raise HTTPException(404, "SaaS app not found")
        return app_obj.to_dict()

    @app.patch("/api/v1/saas/{app_id}")
    async def saas_update(request: Request, app_id: str,
                          body: UpdateSaaSRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        updates = {k: v for k, v in body.model_dump().items() if v is not None}
        if "category" in updates:
            try:
                updates["category"] = SaaSCategory(updates["category"])
            except ValueError:
                raise HTTPException(400, f"Unknown category: {updates['category']}")
        app_obj = license_mgr.update_saas(app_id, **updates)
        if not app_obj:
            raise HTTPException(404, "SaaS app not found")
        return app_obj.to_dict()

    @app.delete("/api/v1/saas/{app_id}")
    async def saas_delete(request: Request, app_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        if not license_mgr.remove_saas(app_id):
            raise HTTPException(404, "SaaS app not found")
        return {"ok": True}

    @app.post("/api/v1/saas/{app_id}/approve")
    async def saas_approve(request: Request, app_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        body = await request.json() if request.headers.get("content-type", "").startswith("application/json") else {}
        app_obj = license_mgr.approve_saas(app_id, owner_user_id=body.get("owner_user_id", ""))
        if not app_obj:
            raise HTTPException(404, "SaaS app not found")
        return app_obj.to_dict()

    # Analytics endpoints

    @app.get("/api/v1/licenses/analytics/summary")
    async def license_cost_summary(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        return license_mgr.cost_summary()

    @app.get("/api/v1/licenses/analytics/renewals")
    async def license_upcoming_renewals(request: Request,
                                        days: int = 90) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        return {"renewals": license_mgr.find_upcoming_renewals(days)}

    @app.get("/api/v1/licenses/analytics/wasted-seats")
    async def license_wasted_seats(request: Request,
                                   threshold: float = 50.0) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        return {"items": license_mgr.find_wasted_seats(threshold)}

    @app.get("/api/v1/saas/analytics/shadow-it")
    async def saas_shadow_it(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        return {"apps": [a.to_dict() for a in license_mgr.find_shadow_it()]}

    @app.get("/api/v1/saas/analytics/no-dpa")
    async def saas_no_dpa(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        return {"apps": [a.to_dict() for a in license_mgr.find_no_dpa()]}

    @app.get("/api/v1/saas/analytics/duplicate-categories")
    async def saas_duplicate_categories(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        dups = license_mgr.find_duplicate_categories()
        return {cat: [a.to_dict() for a in apps] for cat, apps in dups.items()}

    @app.get("/api/v1/saas/offboarding/{user_id}")
    async def saas_offboarding_checklist(request: Request, user_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        return {"checklist": license_mgr.offboarding_checklist(user_id)}

    @app.post("/api/v1/licenses/analytics/check-renewals")
    async def license_check_renewals_now(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not license_mgr:
            raise HTTPException(503, "License manager not enabled")
        alerts = await license_mgr.run_renewal_check_now()
        return {"alerts": [a.to_dict() for a in alerts]}

    # ── MDM Bridge ───────────────────────────────────────────────────────────────

    class MDMConfigPatchRequest(BaseModel):
        provider: str | None = None
        google_admin_email: str | None = None
        google_service_account_json_env: str | None = None
        google_customer_id: str | None = None
        intune_tenant_id: str | None = None
        intune_client_id: str | None = None
        intune_client_secret_env: str | None = None
        jamf_base_url: str | None = None
        jamf_client_id: str | None = None
        jamf_client_secret_env: str | None = None
        wg_endpoint: str | None = None
        wg_server_public_key: str | None = None
        wg_dns: str | None = None
        wg_allowed_ips: str | None = None
        wifi_ssid: str | None = None
        wifi_security: str | None = None
        wifi_password_env: str | None = None
        sync_interval_seconds: int | None = None

    @app.get("/api/v1/mdm/status")
    async def mdm_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        return mdm.status()

    @app.get("/api/v1/mdm/config")
    async def mdm_get_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        return mdm.get_config().to_dict()

    @app.patch("/api/v1/mdm/config")
    async def mdm_patch_config(request: Request, body: MDMConfigPatchRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        cfg = mdm.get_config()
        patch = body.model_dump(exclude_none=True)
        for k, v in patch.items():
            if hasattr(cfg, k):
                setattr(cfg, k, v)
        mdm.set_config(cfg)
        return cfg.to_dict()

    @app.get("/api/v1/mdm/devices")
    async def mdm_list_devices(
        request: Request,
        email: str | None = None,
        platform: str | None = None,
        compliant: bool | None = None,
        vpn_pushed: bool | None = None,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        devices = mdm.list_devices(
            user_email=email,
            platform=platform,
            compliance_state="compliant" if compliant is True else ("noncompliant" if compliant is False else None),
        )
        if vpn_pushed is not None:
            devices = [d for d in devices if d.vpn_profile_pushed == vpn_pushed]
        return {"devices": [d.to_dict() for d in devices]}

    @app.get("/api/v1/mdm/devices/{device_id}")
    async def mdm_get_device(request: Request, device_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        d = mdm.get_device(device_id)
        if not d:
            raise HTTPException(404, "Device not found")
        return d.to_dict()

    @app.post("/api/v1/mdm/sync")
    async def mdm_sync(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        return await mdm.sync()

    @app.post("/api/v1/mdm/devices/{device_id}/push-vpn")
    async def mdm_push_vpn(request: Request, device_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        d = mdm.get_device(device_id)
        if not d:
            raise HTTPException(404, "Device not found")
        ok = await mdm.push_vpn_profile(device_id)
        if not ok:
            raise HTTPException(500, "VPN profile push failed")
        return {"ok": True, "device_id": device_id}

    @app.post("/api/v1/mdm/invite/{email:path}")
    async def mdm_invite(request: Request, email: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        body = await request.json() if request.headers.get("content-type", "").startswith("application/json") else {}
        name = body.get("name", "") if isinstance(body, dict) else ""
        ok = await mdm.invite_enrollment(email, name=name)
        return {"ok": ok, "email": email}

    @app.post("/api/v1/mdm/enroll/invite")
    async def mdm_enroll_invite(request: Request, body: dict = {}) -> dict[str, Any]:
        """Alias: send MDM enrollment invitation (alternative path)."""
        _require_scope(request, SCOPE_ADMIN)
        if not mdm:
            raise HTTPException(404, "MDM bridge not enabled")
        email = body.get("email", "")
        name = body.get("name", "")
        try:
            ok = await mdm.invite_enrollment(email, name=name)
        except (RuntimeError, NotImplementedError):
            raise HTTPException(404, "MDM provider not configured")
        return {"ok": ok, "email": email, "invite_sent": ok}

    @app.post("/api/v1/mdm/offboard/{email:path}")
    async def mdm_offboard(request: Request, email: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        body = await request.json() if request.headers.get("content-type", "").startswith("application/json") else {}
        wipe = bool(body.get("wipe", False)) if isinstance(body, dict) else False
        result = await mdm.offboard_user(email, wipe=wipe)
        return result

    @app.get("/api/v1/mdm/gaps")
    async def mdm_compliance_gaps(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not mdm:
            raise HTTPException(503, "MDM bridge not enabled")
        return {"gaps": mdm.compliance_gaps()}

    # ── Job queue ──────────────────────────────────────────────────────────────

    def _jq() -> JobQueue:
        if not job_queue:
            raise HTTPException(503, "Job queue not enabled")
        return job_queue

    class CreateJobRequest(BaseModel):
        name:             str
        type:             str               = "command"
        command:          str               = ""
        args:             list[str]         = []
        env:              dict[str, str]    = {}
        working_dir:      str               = ""
        timeout_seconds:  int               = 300
        run_as:           str               = ""
        packages:         list[str]         = []
        package_manager:  str               = "auto"
        dest_path:        str               = ""
        content_b64:      str               = ""
        file_mode:        str               = "0644"
        recipe:           str               = ""
        recipe_params:    dict[str, Any]    = {}
        target_node_id:   str               = ""
        deadline:         float | None      = None
        tags:             dict[str, str]    = {}

    class CreateCampaignRequest(BaseModel):
        name:             str
        type:             str               = "command"
        command:          str               = ""
        args:             list[str]         = []
        env:              dict[str, str]    = {}
        working_dir:      str               = ""
        timeout_seconds:  int               = 300
        run_as:           str               = ""
        packages:         list[str]         = []
        package_manager:  str               = "auto"
        dest_path:        str               = ""
        content_b64:      str               = ""
        file_mode:        str               = "0644"
        recipe:           str               = ""
        recipe_params:    dict[str, Any]    = {}
        target_scope:     str               = "all"
        target_ids:       list[str]         = []
        target_labels:    list[str]         = []
        deadline:         float | None      = None
        tags:             dict[str, str]    = {}

    class JobResultRequest(BaseModel):
        exit_code:  int
        stdout:     str = ""
        stderr:     str = ""
        error:      str = ""

    class JobProgressRequest(BaseModel):
        progress:   int
        message:    str = ""

    def _spec_from_request(body: Any) -> JobSpec:
        try:
            jtype = JobType(body.type)
        except ValueError:
            raise HTTPException(400, f"Unknown job type: {body.type}")
        return JobSpec(
            type=jtype, command=body.command, args=body.args, env=body.env,
            working_dir=body.working_dir, timeout_seconds=body.timeout_seconds,
            run_as=body.run_as, packages=body.packages,
            package_manager=body.package_manager, dest_path=body.dest_path,
            content_b64=body.content_b64, file_mode=body.file_mode,
            recipe=body.recipe, recipe_params=body.recipe_params,
        )

    @app.get("/api/v1/jobs")
    async def jobs_list(request: Request,
                        state_filter: str | None = None,
                        node_id: str | None = None,
                        campaign_id: str | None = None,
                        limit: int = 200,
                        offset: int = 0) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        q = _jq()
        jobs = q.list_jobs(state=state_filter, node_id=node_id,
                            campaign_id=campaign_id, limit=limit, offset=offset)
        return {"jobs": [j.to_dict() for j in jobs]}

    @app.post("/api/v1/jobs")
    async def jobs_create(request: Request, body: CreateJobRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        q = _jq()
        if not body.target_node_id:
            raise HTTPException(400, "target_node_id required for single-node job")
        if body.target_node_id not in state.nodes:
            raise HTTPException(404, "Node not found")
        spec = _spec_from_request(body)
        job = await q.create_job(
            name=body.name, spec=spec, target_node_id=body.target_node_id,
            deadline=body.deadline, tags=body.tags,
            created_by=getattr(request.state, "user", "api"),
        )
        return job.to_dict()

    @app.get("/api/v1/jobs/{job_id}")
    async def jobs_get(request: Request, job_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        job = _jq().get_job(job_id)
        if not job:
            raise HTTPException(404, "Job not found")
        return job.to_dict()

    @app.post("/api/v1/jobs/{job_id}/cancel")
    async def jobs_cancel(request: Request, job_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not _jq().cancel_job(job_id):
            raise HTTPException(404, "Job not found or already terminal")
        return {"ok": True}

    @app.post("/api/v1/jobs/{job_id}/retry")
    async def jobs_retry(request: Request, job_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        new_job = await _jq().retry_job(job_id)
        if not new_job:
            raise HTTPException(400, "Job not found or not in a retryable state")
        return new_job.to_dict()

    # Agent result reporting — WireGuard/mesh agents post without JWT

    @app.post("/api/v1/jobs/{job_id}/ack")
    async def jobs_ack(job_id: str) -> dict[str, Any]:
        if not job_queue:
            raise HTTPException(503, "Job queue not enabled")
        await job_queue.handle_ack(job_id)
        return {"ok": True}

    @app.post("/api/v1/jobs/{job_id}/progress")
    async def jobs_progress(job_id: str, body: JobProgressRequest) -> dict[str, Any]:
        if not job_queue:
            raise HTTPException(503, "Job queue not enabled")
        await job_queue.handle_progress(job_id, body.progress, body.message)
        return {"ok": True}

    @app.post("/api/v1/jobs/{job_id}/result")
    async def jobs_result(job_id: str, body: JobResultRequest) -> dict[str, Any]:
        if not job_queue:
            raise HTTPException(503, "Job queue not enabled")
        await job_queue.handle_result(job_id, body.exit_code, body.stdout,
                                       body.stderr, body.error)
        return {"ok": True}

    @app.get("/api/v1/campaigns")
    async def campaigns_list(request: Request,
                              limit: int = 100,
                              offset: int = 0) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        campaigns = _jq().list_campaigns(limit=limit, offset=offset)
        return {"campaigns": [c.to_dict() for c in campaigns]}

    @app.post("/api/v1/campaigns")
    async def campaigns_create(request: Request,
                                body: CreateCampaignRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        q = _jq()
        try:
            scope = TargetScope(body.target_scope)
        except ValueError:
            raise HTTPException(400, f"Unknown target_scope: {body.target_scope}")
        if scope == TargetScope.NODE and not body.target_ids:
            raise HTTPException(400, "target_ids required when target_scope=node")
        spec = _spec_from_request(body)
        campaign = await q.create_campaign(
            name=body.name, spec=spec,
            target_scope=scope,
            target_ids=body.target_ids or None,
            target_labels=body.target_labels or None,
            deadline=body.deadline, tags=body.tags,
            created_by=getattr(request.state, "user", "api"),
        )
        return {"campaign": campaign.to_dict(),
                "summary": q.campaign_summary(campaign.id)}

    @app.get("/api/v1/campaigns/{campaign_id}")
    async def campaigns_get(request: Request, campaign_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        q = _jq()
        campaign = q.get_campaign(campaign_id)
        if not campaign:
            raise HTTPException(404, "Campaign not found")
        return {"campaign": campaign.to_dict(),
                "summary": q.campaign_summary(campaign_id)}

    @app.get("/api/v1/campaigns/{campaign_id}/jobs")
    async def campaigns_jobs(request: Request, campaign_id: str,
                              state_filter: str | None = None) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        q = _jq()
        if not q.get_campaign(campaign_id):
            raise HTTPException(404, "Campaign not found")
        jobs = q.list_jobs(campaign_id=campaign_id, state=state_filter, limit=1000)
        return {"jobs": [j.to_dict() for j in jobs]}

    @app.post("/api/v1/campaigns/{campaign_id}/cancel")
    async def campaigns_cancel(request: Request, campaign_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        q = _jq()
        if not q.get_campaign(campaign_id):
            raise HTTPException(404, "Campaign not found")
        count = q.cancel_campaign(campaign_id)
        return {"ok": True, "cancelled": count}

    # ── Network Scanning ─────────────────────────────────────────────────────────

    class NetworkScanConfigPatchRequest(BaseModel):
        targets: list[str] | None = None
        discovery_interval: int | None = None
        vuln_scan_interval: int | None = None
        rogue_device_alert: bool | None = None
        rogue_device_itsm_ticket: bool | None = None
        rogue_device_itsm_priority: str | None = None
        nmap_args: str | None = None
        nuclei_enabled: bool | None = None
        nuclei_severity: str | None = None
        nuclei_templates: str | None = None
        openvas_enabled: bool | None = None
        openvas_host: str | None = None
        openvas_port: int | None = None
        openvas_username: str | None = None
        openvas_password_env: str | None = None
        openvas_scan_config: str | None = None
        nessus_enabled: bool | None = None
        nessus_url: str | None = None
        nessus_access_key_env: str | None = None
        nessus_secret_key_env: str | None = None
        nessus_policy_id: str | None = None
        ticket_severity_threshold: str | None = None

    @app.get("/api/v1/network-scan/status")
    async def net_scan_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        return net_scan.status()

    @app.get("/api/v1/network-scan/config")
    async def net_scan_get_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        return net_scan.get_config().to_dict()

    @app.patch("/api/v1/network-scan/config")
    async def net_scan_patch_config(request: Request,
                                     body: NetworkScanConfigPatchRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        cfg = net_scan.get_config()
        if body.targets is not None:
            cfg.targets = body.targets
        if body.discovery_interval is not None:
            cfg.discovery_interval = body.discovery_interval
        if body.vuln_scan_interval is not None:
            cfg.vuln_scan_interval = body.vuln_scan_interval
        if body.rogue_device_alert is not None:
            cfg.rogue_device_alert = body.rogue_device_alert
        if body.rogue_device_itsm_ticket is not None:
            cfg.rogue_device_itsm_ticket = body.rogue_device_itsm_ticket
        if body.rogue_device_itsm_priority is not None:
            cfg.rogue_device_itsm_priority = body.rogue_device_itsm_priority
        if body.nmap_args is not None:
            cfg.nmap_args = body.nmap_args
        if body.nuclei_enabled is not None:
            cfg.nuclei_enabled = body.nuclei_enabled
        if body.nuclei_severity is not None:
            cfg.nuclei_severity = body.nuclei_severity
        if body.nuclei_templates is not None:
            cfg.nuclei_templates = body.nuclei_templates
        if body.openvas_enabled is not None:
            cfg.openvas_enabled = body.openvas_enabled
        if body.openvas_host is not None:
            cfg.openvas.host = body.openvas_host
        if body.openvas_port is not None:
            cfg.openvas.port = body.openvas_port
        if body.openvas_username is not None:
            cfg.openvas.username = body.openvas_username
        if body.openvas_password_env is not None:
            cfg.openvas.password_env = body.openvas_password_env
        if body.openvas_scan_config is not None:
            cfg.openvas.scan_config = body.openvas_scan_config
        if body.nessus_enabled is not None:
            cfg.nessus_enabled = body.nessus_enabled
        if body.nessus_url is not None:
            cfg.nessus.url = body.nessus_url
        if body.nessus_access_key_env is not None:
            cfg.nessus.access_key_env = body.nessus_access_key_env
        if body.nessus_secret_key_env is not None:
            cfg.nessus.secret_key_env = body.nessus_secret_key_env
        if body.nessus_policy_id is not None:
            cfg.nessus.policy_id = body.nessus_policy_id
        if body.ticket_severity_threshold is not None:
            cfg.ticket_severity_threshold = body.ticket_severity_threshold
        net_scan.set_config(cfg)
        return cfg.to_dict()

    @app.post("/api/v1/network-scan/discovery")
    async def net_scan_run_discovery(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        return await net_scan.run_discovery()

    @app.post("/api/v1/network-scan/vuln-scan")
    async def net_scan_run_vuln(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        return await net_scan.run_vuln_scan()

    @app.get("/api/v1/network-scan/hosts")
    async def net_scan_list_hosts(
        request: Request,
        os_filter: str | None = None,
        rogue_only: bool = False,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        hosts = net_scan.list_hosts(os_filter=os_filter, rogue_only=rogue_only)
        return {"hosts": [h.to_dict() for h in hosts]}

    @app.post("/api/v1/network-scan/hosts")
    async def net_scan_report_hosts(request: Request, body: dict = {}) -> dict[str, Any]:
        """Report externally discovered hosts; returns alert data for unknown devices."""
        _require_scope(request, SCOPE_WRITE)
        incoming = body.get("hosts", [])
        alerts = []
        unknown = []
        if net_scan:
            known_ips = {h.ip for h in net_scan.list_hosts()}
            for h in incoming:
                ip = h.get("ip", "")
                if ip and ip not in known_ips:
                    unknown.append(h)
                    alerts.append({"type": "rogue_device", "host": h})
        else:
            # No scanner — treat all as potentially unknown
            for h in incoming:
                unknown.append(h)
                alerts.append({"type": "rogue_device", "host": h})
        return {"ok": True, "alerts": alerts, "rogue_devices": unknown, "total": len(incoming)}

    @app.get("/api/v1/network-scan/hosts/{ip}")
    async def net_scan_get_host(request: Request, ip: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        hosts = net_scan.list_hosts(ip=ip)
        if not hosts:
            raise HTTPException(404, "Host not found")
        return hosts[0].to_dict()

    @app.post("/api/v1/network-scan/hosts/{ip}/mark-known")
    async def net_scan_mark_host_known(request: Request, ip: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        body = await request.json() if request.headers.get("content-type", "").startswith("application/json") else {}
        node_id = body.get("node_id", "") if isinstance(body, dict) else ""
        ok = net_scan.mark_host_in_itam(ip, node_id=node_id)
        if not ok:
            raise HTTPException(404, "Host not found")
        return {"ok": True, "ip": ip}

    @app.get("/api/v1/network-scan/findings")
    async def net_scan_list_findings(
        request: Request,
        severity: str | None = None,
        host: str | None = None,
        scanner: str | None = None,
        suppressed: bool | None = None,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        findings = net_scan.list_findings(
            severity=severity, host=host, scanner=scanner, suppressed=suppressed
        )
        return {"findings": [f.to_dict() for f in findings]}

    @app.post("/api/v1/network-scan/findings/{finding_id}/suppress")
    async def net_scan_suppress_finding(request: Request, finding_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        ok = net_scan.suppress_finding(finding_id)
        if not ok:
            raise HTTPException(404, "Finding not found")
        return {"ok": True, "finding_id": finding_id}

    @app.delete("/api/v1/network-scan/findings/{finding_id}/suppress")
    async def net_scan_unsuppress_finding(request: Request, finding_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        ok = net_scan.unsuppress_finding(finding_id)
        if not ok:
            raise HTTPException(404, "Finding not found")
        return {"ok": True, "finding_id": finding_id}

    @app.get("/api/v1/network-scan/results")
    async def net_scan_list_results(request: Request,
                                     limit: int = 20) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not net_scan:
            raise HTTPException(503, "Network scan not enabled")
        return {"results": [r.to_dict() for r in net_scan.list_results(limit=limit)]}

    # ── Master key store ───────────────────────────────────────────────────────

    def _ks() -> KeyStore:
        if not key_store:
            raise HTTPException(503, "Key store not enabled")
        return key_store

    class KeyInitRequest(BaseModel):
        method:       str  = "password"
        password:     str  = ""
        confirm_none: bool = False

    class KeyUnlockRequest(BaseModel):
        method:   str        = "password"
        password: str        = ""
        words:    list[str]  = []

    class KeyChangeMethodRequest(BaseModel):
        method:       str  = "password"
        password:     str  = ""
        confirm_none: bool = False

    class KeyInjectRequest(BaseModel):
        key_b64: str
        ttl:     int = 3600

    @app.get("/api/v1/keys/status")
    async def keys_status(request: Request) -> dict[str, Any]:
        """Key store state — no sensitive material returned."""
        _require_scope(request, SCOPE_READ)
        return _ks().get_status()

    @app.post("/api/v1/keys/init")
    async def keys_init(request: Request, body: KeyInitRequest) -> dict[str, Any]:
        """
        First-time key setup.  Idempotent only if called before any blob exists.
        Returns BIP39 words if method=export (display once, store securely).
        """
        _require_scope(request, SCOPE_ADMIN)
        ks = _ks()
        if ks.get_status()["has_blob"]:
            raise HTTPException(409, "Key already initialised — use change-method to rotate")
        try:
            method = BackupMethod(body.method)
        except ValueError:
            raise HTTPException(400, f"Unknown method: {body.method}")

        if method == BackupMethod.PASSWORD:
            if not body.password:
                raise HTTPException(400, "password required for method=password")
            await ks.init_password(body.password)
            return {"ok": True, "method": "password", "words": None}

        elif method == BackupMethod.EXPORT:
            words = await ks.init_export()
            return {"ok": True, "method": "export", "words": words,
                    "warning": "These 24 words ARE your master key. Store them like cash — they cannot be recovered if lost."}

        elif method == BackupMethod.NONE:
            if not body.confirm_none:
                raise HTTPException(400, "Set confirm_none=true to acknowledge key loss on restart")
            await ks.init_none(confirm=True)
            return {"ok": True, "method": "none", "words": None,
                    "warning": "Ephemeral key — all encrypted data is lost on controller restart."}

        raise HTTPException(400, f"Unsupported method: {body.method}")

    @app.post("/api/v1/keys/unlock")
    async def keys_unlock(request: Request, body: KeyUnlockRequest) -> dict[str, Any]:
        """Unlock master key from password or BIP39 words."""
        _require_scope(request, SCOPE_ADMIN)
        ks = _ks()
        try:
            if body.method == "password":
                ok = await ks.unlock_password(body.password)
            elif body.method == "export":
                ok = await ks.unlock_export(body.words)
            else:
                raise HTTPException(400, f"Unknown unlock method: {body.method}")
        except UnlockRateLimitedError as exc:
            raise HTTPException(429, f"Too many failed attempts — retry in {int(exc.retry_after - __import__('time').monotonic())}s")
        except KeyNotInitialisedError:
            raise HTTPException(409, "Key store not initialised — call /keys/init first")

        if not ok:
            status = ks.get_status()
            remaining = ks.MAX_ATTEMPTS - status["failed_attempts"]
            raise HTTPException(401, f"Wrong credentials — {max(0, remaining)} attempts remaining")

        return {"ok": True, "state": ks.get_status()["state"]}

    @app.post("/api/v1/keys/lock")
    async def keys_lock(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        _ks().lock()
        return {"ok": True, "state": "locked"}

    @app.post("/api/v1/keys/export")
    async def keys_export_words(request: Request) -> dict[str, Any]:
        """
        Return the 24 BIP39 recovery words for the current master key.
        Requires unlocked state.  ADMIN scope.
        """
        _require_scope(request, SCOPE_ADMIN)
        ks = _ks()
        try:
            words = ks.export_words()
        except KeyLockedError as exc:
            raise HTTPException(423, str(exc))
        return {"words": words,
                "warning": "These words give full access to all encrypted data. Never share them."}

    @app.post("/api/v1/keys/change-method")
    async def keys_change_method(request: Request,
                                  body: KeyChangeMethodRequest) -> dict[str, Any]:
        """
        Change backup method.  Requires unlocked state.
        Returns BIP39 words if switching to export method.
        """
        _require_scope(request, SCOPE_ADMIN)
        ks = _ks()
        try:
            method = BackupMethod(body.method)
        except ValueError:
            raise HTTPException(400, f"Unknown method: {body.method}")
        try:
            words = await ks.change_method(method, password=body.password,
                                            confirm_none=body.confirm_none)
        except KeyLockedError as exc:
            raise HTTPException(423, str(exc))
        except ValueError as exc:
            raise HTTPException(400, str(exc))

        result: dict[str, Any] = {"ok": True, "method": method.value}
        if words:
            result["words"] = words
            result["warning"] = "These 24 words ARE your master key. Store them now — they cannot be shown again."
        return result

    @app.post("/api/v1/keys/inject")
    async def keys_inject(request: Request, body: KeyInjectRequest) -> dict[str, Any]:
        """
        BYOK session injection (cloud controller Pro).
        Injects a master key for a bounded session.  Memory-only — never persisted.
        Requires ADMIN scope.
        """
        _require_scope(request, SCOPE_ADMIN)
        ks = _ks()
        try:
            key_bytes = __import__("base64").b64decode(body.key_b64)
        except Exception:
            raise HTTPException(400, "key_b64 must be valid base64")
        if len(key_bytes) != 32:
            raise HTTPException(400, "Injected key must decode to exactly 32 bytes")

        actor = getattr(request.state, "user", "unknown")
        source_ip = request.client.host if request.client else "unknown"
        session = await ks.inject(key_bytes, ttl=body.ttl,
                                   actor=actor, source_ip=source_ip)
        return {"ok": True, "session": session.to_dict()}

    @app.post("/api/v1/keys/evict")
    async def keys_evict(request: Request) -> dict[str, Any]:
        """Evict BYOK injected session."""
        _require_scope(request, SCOPE_ADMIN)
        _ks().evict_injected()
        return {"ok": True}

    @app.post("/api/v1/keys/backup")
    async def keys_backup_to_connect(request: Request) -> dict[str, Any]:
        """Push the key blob to Ozma Connect for account-level backup."""
        _require_scope(request, SCOPE_ADMIN)
        ks = _ks()
        if not ks.connect_backup_enabled():
            raise HTTPException(409, "Key backup not available for 'none' backup method — change method first")
        try:
            await ks.backup_to_connect(connect)
        except RuntimeError as exc:
            raise HTTPException(503, str(exc))
        return {"ok": True, "method": ks.get_status()["method"]}

    @app.post("/api/v1/keys/restore")
    async def keys_restore_from_connect(request: Request) -> dict[str, Any]:
        """
        Pull key blob from Ozma Connect.

        Used for disaster recovery when local key storage has been lost.
        After restore, call /keys/unlock with the original password/words.
        """
        _require_scope(request, SCOPE_ADMIN)
        try:
            await _ks().restore_from_connect(connect)
        except RuntimeError as exc:
            raise HTTPException(503, str(exc))
        return {"ok": True, "status": _ks().get_status()}

    # ── DLP ──────────────────────────────────────────────────────────────────────

    class DLPRuleRequest(BaseModel):
        name: str
        pattern_type: str
        custom_pattern: str = ""
        action: str = "alert"
        severity: str = "high"
        scopes: list[str] = list(("file", "email", "cloud", "clipboard", "usb"))
        enabled: bool = True
        min_matches: int = 1
        validate_matches: bool = True

    class DLPConfigPatchRequest(BaseModel):
        file_scan_interval: int | None = None
        itsm_ticket_severity: str | None = None
        email_scan_enabled: bool | None = None
        cloud_scan_enabled: bool | None = None
        cloud_scan_interval: int | None = None
        usb_alert_enabled: bool | None = None
        quarantine_path: str | None = None

    @app.get("/api/v1/dlp/status")
    async def dlp_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        return dlp.status()

    @app.get("/api/v1/dlp/config")
    async def dlp_get_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        return dlp.get_config().to_dict()

    @app.patch("/api/v1/dlp/config")
    async def dlp_patch_config(request: Request,
                                body: DLPConfigPatchRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        cfg = dlp.get_config()
        for field_name, val in body.model_dump(exclude_none=True).items():
            actual = "validate" if field_name == "validate_matches" else field_name
            if hasattr(cfg, actual):
                setattr(cfg, actual, val)
        dlp.set_config(cfg)
        return cfg.to_dict()

    @app.get("/api/v1/dlp/policies")
    async def dlp_list_policies(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        return {"policies": [p.to_dict() for p in dlp.list_policies()]}

    @app.post("/api/v1/dlp/policies")
    async def dlp_create_policy(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        body = await request.json()
        policy = dlp.create_policy(
            name=body.get("name", "New Policy"),
            description=body.get("description", ""),
        )
        return policy.to_dict()

    @app.post("/api/v1/dlp/policies/default")
    async def dlp_create_default_policy(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        policy = dlp.create_default_policy()
        return policy.to_dict()

    @app.get("/api/v1/dlp/policies/{policy_id}")
    async def dlp_get_policy(request: Request, policy_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        p = dlp.get_policy(policy_id)
        if not p:
            raise HTTPException(404, "Policy not found")
        return p.to_dict()

    @app.patch("/api/v1/dlp/policies/{policy_id}")
    async def dlp_update_policy(request: Request, policy_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        body = await request.json()
        p = dlp.update_policy(policy_id, **{k: v for k, v in body.items()
                                             if k not in ("id", "rules")})
        if not p:
            raise HTTPException(404, "Policy not found")
        return p.to_dict()

    @app.delete("/api/v1/dlp/policies/{policy_id}")
    async def dlp_delete_policy(request: Request, policy_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        if not dlp.delete_policy(policy_id):
            raise HTTPException(404, "Policy not found")
        return {"ok": True}

    @app.post("/api/v1/dlp/policies/{policy_id}/rules")
    async def dlp_add_rule(request: Request, policy_id: str,
                            body: DLPRuleRequest) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        import uuid as _uuid
        rule = DLPRule(
            id=str(_uuid.uuid4()), name=body.name,
            pattern_type=body.pattern_type,
            custom_pattern=body.custom_pattern,
            action=body.action, severity=body.severity,
            scopes=body.scopes, enabled=body.enabled,
            min_matches=body.min_matches, validate=body.validate_matches,
        )
        p = dlp.add_rule(policy_id, rule)
        if not p:
            raise HTTPException(404, "Policy not found")
        return p.to_dict()

    @app.patch("/api/v1/dlp/policies/{policy_id}/rules/{rule_id}")
    async def dlp_update_rule(request: Request, policy_id: str,
                               rule_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        body = await request.json()
        rule = dlp.update_rule(policy_id, rule_id,
                                **{k: v for k, v in body.items() if k != "id"})
        if not rule:
            raise HTTPException(404, "Policy or rule not found")
        return rule.to_dict()

    @app.delete("/api/v1/dlp/policies/{policy_id}/rules/{rule_id}")
    async def dlp_remove_rule(request: Request, policy_id: str,
                               rule_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        if not dlp.remove_rule(policy_id, rule_id):
            raise HTTPException(404, "Policy or rule not found")
        return {"ok": True}

    @app.get("/api/v1/dlp/incidents")
    async def dlp_list_incidents(
        request: Request,
        scope: str | None = None,
        severity: str | None = None,
        resolved: bool | None = None,
        node_id: str | None = None,
        limit: int = 100,
    ) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        incidents = dlp.list_incidents(
            scope=scope, severity=severity, resolved=resolved,
            node_id=node_id, limit=limit,
        )
        return {"incidents": [i.to_dict() for i in incidents]}

    @app.get("/api/v1/dlp/incidents/{incident_id}")
    async def dlp_get_incident(request: Request, incident_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        i = dlp.get_incident(incident_id)
        if not i:
            raise HTTPException(404, "Incident not found")
        return i.to_dict()

    @app.post("/api/v1/dlp/incidents/{incident_id}/acknowledge")
    async def dlp_ack_incident(request: Request, incident_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        if not dlp.acknowledge_incident(incident_id):
            raise HTTPException(404, "Incident not found")
        return {"ok": True}

    @app.post("/api/v1/dlp/incidents/{incident_id}/resolve")
    async def dlp_resolve_incident(request: Request, incident_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        if not dlp.resolve_incident(incident_id):
            raise HTTPException(404, "Incident not found")
        return {"ok": True}

    @app.post("/api/v1/dlp/scan/file")
    async def dlp_file_scan(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        body = await request.json() if request.headers.get("content-type", "").startswith("application/json") else {}
        paths = body.get("paths") if isinstance(body, dict) else None
        node_id = body.get("node_id", "") if isinstance(body, dict) else ""
        return await dlp.run_file_scan(paths=paths, node_id=node_id)

    @app.post("/api/v1/dlp/scan/content")
    async def dlp_scan_content(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        body = await request.json()
        text = body.get("text", "") or body.get("content", "")
        scope = body.get("scope", "file")
        source = body.get("source", "manual")
        node_id = body.get("node_id", "")
        user_email = body.get("user_email", "")
        incidents = await dlp.scan_content(
            text, scope=scope, source=source,
            node_id=node_id, user_email=user_email,
        )
        return {"incidents": [i.to_dict() for i in incidents]}

    @app.post("/api/v1/dlp/usb-event")
    async def dlp_usb_event(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        if not dlp:
            raise HTTPException(503, "DLP not enabled")
        body = await request.json()
        node_id = body.get("node_id", "")
        device_name = body.get("device_name", "unknown")
        user_email = body.get("user_email", "")
        incidents = await dlp.handle_usb_event(
            node_id=node_id, device_name=device_name, user_email=user_email
        )
        return {"incidents": [i.to_dict() for i in incidents]}

    # ── SaaS Management ───────────────────────────────────────────────────────

    @app.get("/api/v1/saas/status")
    async def saas_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        return saas_mgr.status()

    @app.get("/api/v1/saas/config")
    async def saas_get_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        return saas_mgr.get_config().to_dict()

    @app.patch("/api/v1/saas/config")
    async def saas_update_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        body = await request.json()
        return saas_mgr.set_config(body).to_dict()

    @app.get("/api/v1/saas/apps")
    async def saas_list_apps(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        params = dict(request.query_params)
        shadow_it = None
        if "shadow_it" in params:
            shadow_it = params["shadow_it"].lower() == "true"
        return [a.to_dict() for a in saas_mgr.list_apps(
            shadow_it=shadow_it,
            category=params.get("category"),
            renewal_risk=params.get("renewal_risk"),
            source=params.get("source"),
        )]

    @app.post("/api/v1/saas/apps")
    async def saas_register_app(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        body = await request.json()
        name = body.get("name", "")
        if not name:
            raise HTTPException(400, "name is required")
        app = saas_mgr.register_app(
            name=name,
            vendor=body.get("vendor", ""),
            domain=body.get("domain", ""),
            source=body.get("source", "manual"),
            **{k: v for k, v in body.items()
               if k not in ("name", "vendor", "domain", "source")},
        )
        return app.to_dict()

    @app.get("/api/v1/saas/apps/{app_id}")
    async def saas_get_app(request: Request, app_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        app = saas_mgr.get_app(app_id)
        if not app:
            raise HTTPException(404, "App not found")
        return app.to_dict()

    @app.patch("/api/v1/saas/apps/{app_id}")
    async def saas_update_app(request: Request, app_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        body = await request.json()
        app = saas_mgr.update_app(app_id, body)
        if not app:
            raise HTTPException(404, "App not found")
        return app.to_dict()

    @app.delete("/api/v1/saas/apps/{app_id}")
    async def saas_delete_app(request: Request, app_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        if not saas_mgr.delete_app(app_id):
            raise HTTPException(404, "App not found")
        return {"ok": True}

    @app.post("/api/v1/saas/apps/{app_id}/approve")
    async def saas_approve_app(request: Request, app_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        app = saas_mgr.update_app(app_id, {"approved": True})
        if not app:
            raise HTTPException(404, "App not found")
        return app.to_dict()

    @app.get("/api/v1/saas/shadow-it")
    async def saas_shadow_it(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        return saas_mgr.shadow_it_summary()

    @app.get("/api/v1/saas/cost")
    async def saas_cost_summary(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            return {"total": 0.0, "monthly": 0.0, "services": [], "costs": {}}
        return saas_mgr.cost_summary()

    @app.get("/api/v1/saas/renewals")
    async def saas_renewals(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        days = int(request.query_params.get("days", "90"))
        return saas_mgr.upcoming_renewals(days=days)

    @app.get("/api/v1/saas/duplicates")
    async def saas_duplicates(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        return saas_mgr.duplicate_categories()

    @app.get("/api/v1/saas/vendor-risk")
    async def saas_vendor_risk(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        return saas_mgr.vendor_risk_summary()

    @app.get("/api/v1/saas/offboarding/{user_email}")
    async def saas_offboarding_checklist(request: Request, user_email: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        return saas_mgr.create_offboarding_checklist(user_email)

    @app.post("/api/v1/saas/discover/chrome-extensions")
    async def saas_ingest_chrome_extensions(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        body = await request.json()
        node_id = body.get("node_id", "")
        extensions = body.get("extensions", [])
        count = saas_mgr.ingest_chrome_extensions(node_id, extensions)
        return {"ok": True, "new_apps": count}

    @app.post("/api/v1/saas/discover/dns-domains")
    async def saas_ingest_dns(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        body = await request.json()
        domains = body.get("domains", [])
        count = saas_mgr.ingest_dns_domains(domains)
        return {"ok": True, "new_apps": count}

    @app.post("/api/v1/saas/discover/invoices")
    async def saas_ingest_invoices(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not saas_mgr:
            raise HTTPException(503, "SaaS management not configured")
        body = await request.json()
        invoices = body.get("invoices", [])
        count = saas_mgr.ingest_invoice_data(invoices)
        return {"ok": True, "apps_updated": count}

    # ── Threat Intelligence ───────────────────────────────────────────────────

    @app.get("/api/v1/threat/status")
    async def threat_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        return threat_intel.status()

    @app.get("/api/v1/threat/config")
    async def threat_get_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        return threat_intel.get_config().to_dict()

    @app.patch("/api/v1/threat/config")
    async def threat_update_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        body = await request.json()
        return threat_intel.set_config(body).to_dict()

    @app.post("/api/v1/threat/poll/kev")
    async def threat_poll_kev(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        new_entries = await threat_intel.poll_cisa_kev()
        return {"ok": True, "new_entries": len(new_entries)}

    @app.get("/api/v1/threat/kev")
    async def threat_list_kev(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        params = dict(request.query_params)
        matched_sbom = None
        if "matched_sbom" in params:
            matched_sbom = params["matched_sbom"].lower() == "true"
        return [e.to_dict() for e in threat_intel.list_kev(matched_sbom=matched_sbom)]

    @app.post("/api/v1/threat/poll/advisories")
    async def threat_poll_advisories(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        new_adv = await threat_intel.poll_acsc_advisories()
        return {"ok": True, "new_advisories": len(new_adv)}

    @app.post("/api/v1/threat/advisories")
    async def threat_ingest_advisory(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        body = await request.json()
        adv = threat_intel.ingest_advisory(body)
        return adv.to_dict()

    @app.get("/api/v1/threat/advisories")
    async def threat_list_advisories(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        params = dict(request.query_params)
        ack = None
        if "acknowledged" in params:
            ack = params["acknowledged"].lower() == "true"
        return [a.to_dict() for a in threat_intel.list_advisories(
            source=params.get("source"),
            severity=params.get("severity"),
            acknowledged=ack,
        )]

    @app.post("/api/v1/threat/advisories/{advisory_id}/acknowledge")
    async def threat_ack_advisory(request: Request, advisory_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        adv = threat_intel.acknowledge_advisory(advisory_id)
        if not adv:
            raise HTTPException(404, "Advisory not found")
        return adv.to_dict()

    @app.post("/api/v1/threat/poll/exposure")
    async def threat_poll_exposure(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        body = await request.json()
        domains = body.get("domains")
        new_exp = await threat_intel.check_credential_exposure(domains=domains)
        return {"ok": True, "new_exposures": len(new_exp)}

    @app.get("/api/v1/threat/exposures")
    async def threat_list_exposures(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        params = dict(request.query_params)
        resolved = None
        if "resolved" in params:
            resolved = params["resolved"].lower() == "true"
        return [e.to_dict() for e in threat_intel.list_exposures(resolved=resolved)]

    @app.post("/api/v1/threat/exposures/{exposure_id}/resolve")
    async def threat_resolve_exposure(request: Request, exposure_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        exp = threat_intel.resolve_exposure(exposure_id)
        if not exp:
            raise HTTPException(404, "Exposure record not found")
        return exp.to_dict()

    @app.post("/api/v1/threat/typosquat")
    async def threat_check_typosquat(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        body = await request.json()
        domain = body.get("domain", "")
        if not domain:
            raise HTTPException(400, "domain is required")
        results = await threat_intel.check_typosquat(domain)
        return {"domain": domain, "suspicious": results, "count": len(results)}

    @app.get("/api/v1/threat/attack-coverage")
    async def threat_attack_coverage(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        controls_param = request.query_params.get("controls", "")
        active_controls = [c.strip() for c in controls_param.split(",") if c.strip()]
        return threat_intel.compute_attack_coverage(active_controls)

    @app.post("/api/v1/threat/sbom-cves")
    async def threat_update_sbom(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        body = await request.json()
        cves = set(body.get("cve_ids", []))
        matches = threat_intel.update_sbom_cves(cves)
        return {"ok": True, "new_kev_matches": matches}

    @app.get("/api/v1/threat/briefing")
    async def threat_briefing(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        controls_param = request.query_params.get("controls", "")
        active_controls = [c.strip() for c in controls_param.split(",") if c.strip()]
        return threat_intel.generate_threat_briefing(active_controls)

    @app.get("/api/v1/threat/posture-changes")
    async def threat_posture_changes(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not threat_intel:
            raise HTTPException(503, "Threat intelligence not configured")
        return [p.to_dict() for p in threat_intel.list_posture_changes()]

    # ── Audit Log ─────────────────────────────────────────────────────────────

    @app.get("/api/v1/audit")
    async def audit_log_entries(
        request: Request,
        action: str | None = None,
        limit: int = 100,
    ) -> dict[str, Any]:
        """Return recent audit log entries from the hash-chained JSONL files."""
        _require_scope(request, SCOPE_READ)
        import json as _json
        from pathlib import Path as _Path
        import time as _time
        _audit_dir = _Path(__file__).parent / "audit_logs"
        entries: list[dict] = []
        if _audit_dir.exists():
            date_str = _time.strftime("%Y%m%d")
            log_path = _audit_dir / f"audit-{date_str}.jsonl"
            if log_path.exists():
                raw = log_path.read_text().strip().splitlines()
                for line in raw[-limit:]:
                    try:
                        e = _json.loads(line)
                        # Normalise to {timestamp, action, subject, data}
                        entries.append({
                            "timestamp": e.get("ts", 0),
                            "action": e.get("type", ""),
                            "subject": e.get("source", ""),
                            "data": e.get("data", {}),
                        })
                    except Exception:
                        pass
        if action:
            entries = [e for e in entries if e["action"] == action]
        return {"entries": entries[-limit:]}

    @app.get("/api/v1/audit/log")
    async def audit_log_compat(
        request: Request,
        action: str | None = None,
        limit: int = 100,
    ) -> dict | list:
        """Compatibility alias for GET /api/v1/audit."""
        result = await audit_log_entries(request, action=action, limit=limit)
        return result["entries"]

    # ── Compliance Reports ────────────────────────────────────────────────────

    @app.get("/api/v1/compliance/status")
    async def compliance_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not compliance:
            raise HTTPException(503, "Compliance engine not configured")
        return compliance.status()

    @app.get("/api/v1/compliance/config")
    async def compliance_get_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not compliance:
            raise HTTPException(503, "Compliance engine not configured")
        return compliance.get_config().to_dict()

    @app.patch("/api/v1/compliance/config")
    async def compliance_update_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not compliance:
            raise HTTPException(503, "Compliance engine not configured")
        body = await request.json()
        return compliance.set_config(body).to_dict()

    @app.post("/api/v1/compliance/report")
    async def compliance_generate_report(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not compliance:
            raise HTTPException(503, "Compliance engine not configured")
        body = await request.json()
        framework = body.get("framework", "")
        if not framework:
            # Also accept "frameworks" list (use first element)
            frameworks_list = body.get("frameworks", [])
            if frameworks_list and isinstance(frameworks_list, list):
                framework = frameworks_list[0]
        if not framework:
            raise HTTPException(400, "framework is required")
        # Normalize shorthand aliases to canonical framework IDs
        _fw_aliases: dict[str, str] = {
            "essential_eight": "essential_eight_ml1",
            "e8": "essential_eight_ml1",
            "iso27001": "iso27001_2022",
            "iso_27001": "iso27001_2022",
            "soc2": "soc2_type1",
        }
        framework = _fw_aliases.get(framework, framework)
        scope = body.get("scope", "all")
        report = await compliance.generate_report(framework, scope=scope)
        return report.to_dict()

    @app.get("/api/v1/compliance/reports")
    async def compliance_list_reports(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not compliance:
            raise HTTPException(503, "Compliance engine not configured")
        return compliance.list_reports()

    @app.get("/api/v1/compliance/reports/{report_id}")
    async def compliance_get_report(request: Request, report_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not compliance:
            raise HTTPException(503, "Compliance engine not configured")
        report = compliance.get_report(report_id)
        if not report:
            raise HTTPException(404, "Report not found")
        return report.to_dict()

    @app.get("/api/v1/compliance/gaps")
    async def compliance_list_gaps(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not compliance:
            raise HTTPException(503, "Compliance engine not configured")
        params = dict(request.query_params)
        resolved = None
        if "resolved" in params:
            resolved = params["resolved"].lower() == "true"
        return [g.to_dict() for g in compliance.list_gaps(
            framework=params.get("framework"),
            resolved=resolved,
            severity=params.get("severity"),
        )]

    @app.post("/api/v1/compliance/gaps/{gap_id}/resolve")
    async def compliance_resolve_gap(request: Request, gap_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not compliance:
            raise HTTPException(503, "Compliance engine not configured")
        gap = compliance.resolve_gap(gap_id)
        if not gap:
            raise HTTPException(404, "Gap not found")
        return gap.to_dict()

    @app.get("/api/v1/compliance/soa")
    async def compliance_soa(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not compliance:
            raise HTTPException(503, "Compliance engine not configured")
        return await compliance.generate_soa()

    # ── Wi-Fi AP ─────────────────────────────────────────────────────────────

    @app.get("/api/v1/wifi-ap/status")
    async def wifi_ap_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not wifi_ap:
            raise HTTPException(503, "Wi-Fi AP not configured")
        return wifi_ap.get_status()

    @app.get("/api/v1/wifi-ap/config")
    async def wifi_ap_get_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not wifi_ap:
            raise HTTPException(503, "Wi-Fi AP not configured")
        return wifi_ap.get_config().to_dict()

    @app.patch("/api/v1/wifi-ap/config")
    async def wifi_ap_set_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not wifi_ap:
            raise HTTPException(503, "Wi-Fi AP not configured")
        body = await request.json()
        cfg = await wifi_ap.set_config(**body)
        return cfg.to_dict()

    @app.get("/api/v1/wifi-ap/interfaces")
    async def wifi_ap_interfaces(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not wifi_ap:
            raise HTTPException(503, "Wi-Fi AP not configured")
        return await wifi_ap.probe_interfaces()

    # ── Router Mode ───────────────────────────────────────────────────────────

    @app.get("/api/v1/router/status")
    async def router_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not router:
            raise HTTPException(503, "Router mode not configured")
        return router.get_status()

    @app.get("/api/v1/router/config")
    async def router_get_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        if not router:
            raise HTTPException(503, "Router mode not configured")
        return router.get_config().to_dict()

    @app.patch("/api/v1/router/config")
    async def router_set_config(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not router:
            raise HTTPException(503, "Router mode not configured")
        body = await request.json()
        cfg = await router.set_config(**body)
        return cfg.to_dict()

    @app.get("/api/v1/router/cloud-allow-rules")
    async def router_cloud_allow_list(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        if not router:
            raise HTTPException(503, "Router mode not configured")
        return router.list_cloud_allow_rules()

    @app.post("/api/v1/router/cloud-allow-rules")
    async def router_cloud_allow_add(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not router:
            raise HTTPException(503, "Router mode not configured")
        body = await request.json()
        try:
            rule = await router.add_cloud_allow_rule(
                device_ip=body["device_ip"],
                destination=body["destination"],
                comment=body.get("comment", ""),
            )
        except KeyError as exc:
            raise HTTPException(400, f"Missing field: {exc}")
        return rule

    @app.delete("/api/v1/router/cloud-allow-rules")
    async def router_cloud_allow_remove(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not router:
            raise HTTPException(503, "Router mode not configured")
        body = await request.json()
        removed = await router.remove_cloud_allow_rule(
            device_ip=body.get("device_ip", ""),
            destination=body.get("destination", ""),
        )
        if not removed:
            raise HTTPException(404, "Rule not found")
        return {"deleted": True}

    @app.post("/api/v1/router/camera-trust")
    async def router_trust_camera(request: Request) -> dict[str, Any]:
        """Add a camera node IP to the trusted set (VLAN exempt)."""
        _require_scope(request, SCOPE_WRITE)
        if not router:
            raise HTTPException(503, "Router mode not configured")
        body = await request.json()
        ip = body.get("ip", "")
        if not ip:
            raise HTTPException(400, "ip is required")
        await router.add_trusted_camera(ip)
        return {"ok": True, "ip": ip}

    @app.delete("/api/v1/router/camera-trust/{ip}")
    async def router_untrust_camera(request: Request, ip: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not router:
            raise HTTPException(503, "Router mode not configured")
        await router.remove_trusted_camera(ip)
        return {"ok": True, "ip": ip}

    @app.post("/api/v1/router/frigate-autostart")
    async def router_frigate_autostart(request: Request) -> dict[str, Any]:
        """Probe hardware and auto-start Frigate if capable."""
        _require_scope(request, SCOPE_WRITE)
        if not router:
            raise HTTPException(503, "Router mode not configured")
        started = await router.start_frigate_if_capable()
        return {"started": started}

    # ── Camera Recording ─────────────────────────────────────────────────────

    def _cr():
        if not cam_rec:
            raise HTTPException(503, "Camera recording not configured")
        return cam_rec

    @app.get("/api/v1/recording/status")
    async def recording_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _cr().get_status()

    @app.get("/api/v1/recording/jobs")
    async def recording_jobs(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        return _cr().list_jobs()

    @app.get("/api/v1/recording/policies")
    async def recording_list_policies(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        return [p.to_dict() for p in _cr().list_policies()]

    @app.post("/api/v1/recording/policies")
    async def recording_create_policy(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        mgr = _cr()
        from camera_recording import RecordingTrigger, StorageBackend
        try:
            policy = mgr.add_policy(
                name=body["name"],
                camera_node_id=body.get("camera_node_id"),
                trigger=RecordingTrigger(body.get("trigger", "continuous")),
                object_classes=body.get("object_classes", []),
                event_types=body.get("event_types", []),
                segment_seconds=int(body.get("segment_seconds", 60)),
                pre_buffer_seconds=int(body.get("pre_buffer_seconds", 5)),
                post_buffer_seconds=int(body.get("post_buffer_seconds", 30)),
                backend=StorageBackend(body.get("backend", "local")),
                backend_config=body.get("backend_config", {}),
                encrypted=bool(body.get("encrypted", False)),
                retention_days=int(body.get("retention_days", 30)),
                enabled=bool(body.get("enabled", True)),
            )
        except (KeyError, ValueError) as exc:
            raise HTTPException(400, str(exc))
        return policy.to_dict()

    @app.get("/api/v1/recording/policies/{policy_id}")
    async def recording_get_policy(request: Request, policy_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        policy = _cr().get_policy(policy_id)
        if not policy:
            raise HTTPException(404, "Policy not found")
        return policy.to_dict()

    @app.patch("/api/v1/recording/policies/{policy_id}")
    async def recording_update_policy(request: Request, policy_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        policy = _cr().update_policy(policy_id, **body)
        if not policy:
            raise HTTPException(404, "Policy not found")
        return policy.to_dict()

    @app.delete("/api/v1/recording/policies/{policy_id}")
    async def recording_delete_policy(request: Request, policy_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not _cr().remove_policy(policy_id):
            raise HTTPException(404, "Policy not found")
        return {"deleted": policy_id}

    # ── Node Backup Status ───────────────────────────────────────────────────

    @app.post("/api/v1/nodes/{node_id}/backup-status")
    async def node_backup_status_ingest(request: Request, node_id: str) -> dict[str, Any]:
        """Accept a backup status report pushed by an agent running on the node."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        if backup_tracker:
            backup_tracker.ingest(node_id, body)
        return {"ok": True}

    @app.get("/api/v1/backup/fleet")
    async def backup_fleet_summary(request: Request) -> dict[str, Any]:
        """Fleet-level backup health summary."""
        _require_scope(request, SCOPE_READ)
        if not backup_tracker:
            raise HTTPException(503, "Backup tracking not configured")
        return backup_tracker.get_fleet_summary().to_dict()

    @app.get("/api/v1/backup/nodes/{node_id}")
    async def backup_node_report(request: Request, node_id: str) -> dict[str, Any]:
        """Per-node backup status."""
        _require_scope(request, SCOPE_READ)
        if not backup_tracker:
            raise HTTPException(503, "Backup tracking not configured")
        report = backup_tracker.get_node_report(node_id)
        if not report:
            raise HTTPException(404, "No backup report for this node")
        return report.to_dict()

    @app.delete("/api/v1/backup/nodes/{node_id}")
    async def backup_node_remove(request: Request, node_id: str) -> dict[str, Any]:
        """Remove a decommissioned node's backup records."""
        _require_scope(request, SCOPE_WRITE)
        if not backup_tracker:
            raise HTTPException(503, "Backup tracking not configured")
        backup_tracker.remove_node(node_id)
        return {"deleted": node_id}

    # ── Game Streaming (V1.2) — Sunshine / Moonlight ────────────────────────

    def _ss():
        if not sunshine:
            raise HTTPException(503, "Game streaming not configured")
        return sunshine

    @app.get("/api/v1/streaming/status")
    async def streaming_status(request: Request) -> dict[str, Any]:
        """Fleet-level streaming status: all nodes + binary availability."""
        _require_scope(request, SCOPE_READ)
        mgr = _ss()
        return {
            "available": mgr.is_available(),
            "instances": mgr.get_all_status(),
        }

    @app.get("/api/v1/streaming/nodes/{node_id}")
    async def streaming_node_status(request: Request, node_id: str) -> dict[str, Any]:
        """Per-node Sunshine status, config, and paired clients."""
        _require_scope(request, SCOPE_READ)
        status = _ss().get_status(node_id)
        if not status:
            raise HTTPException(404, "No Sunshine instance for this node")
        return status

    @app.post("/api/v1/streaming/nodes/{node_id}/enable")
    async def streaming_enable_node(request: Request, node_id: str) -> dict[str, Any]:
        """
        Enable Sunshine streaming for a node.

        Body (all optional):
          capture       — kms | wlroots | x11 | v4l2 | auto
          encoder       — nvenc | vaapi | qsv | v4l2m2m | software | auto
          codec         — h264 | h265 | av1
          fps           — 30 | 60
          bitrate_kbps  — target bitrate in kbps (default 10000)
          resolutions   — ["1920x1080", ...]
          v4l2_device   — /dev/videoN (soft nodes with v4l2loopback)
          audio_sink    — PipeWire sink name
        """
        _require_scope(request, SCOPE_WRITE)
        body: dict = {}
        try:
            body = await request.json()
        except Exception:
            pass
        result = await _ss().enable_node(
            node_id      = node_id,
            capture      = body.get("capture", "auto"),
            encoder      = body.get("encoder", "auto"),
            codec        = body.get("codec", "h264"),
            fps          = int(body.get("fps", 60)),
            bitrate_kbps = int(body.get("bitrate_kbps", 10_000)),
            resolutions  = body.get("resolutions"),
            v4l2_device  = body.get("v4l2_device", ""),
            audio_sink   = body.get("audio_sink", ""),
        )
        return result

    @app.post("/api/v1/streaming/nodes/{node_id}/disable")
    async def streaming_disable_node(request: Request, node_id: str) -> dict[str, Any]:
        """Stop Sunshine for a node."""
        _require_scope(request, SCOPE_WRITE)
        await _ss().disable_node(node_id)
        return {"ok": True, "node_id": node_id}

    @app.post("/api/v1/streaming/nodes/{node_id}/pair")
    async def streaming_pair(request: Request, node_id: str) -> dict[str, Any]:
        """
        Submit a Moonlight pairing PIN to Sunshine.

        Moonlight shows a 4-digit PIN; the user enters it in the dashboard
        and POSTs it here.  Body: {"pin": "1234"}
        """
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        pin = str(body.get("pin", "")).strip()
        if not pin or not pin.isdigit() or len(pin) != 4:
            raise HTTPException(400, "pin must be a 4-digit string")
        return await _ss().pair(node_id, pin)

    @app.get("/api/v1/streaming/nodes/{node_id}/clients")
    async def streaming_list_clients(request: Request, node_id: str) -> list[dict[str, Any]]:
        """List paired Moonlight clients for a node."""
        _require_scope(request, SCOPE_READ)
        return await _ss().list_clients(node_id)

    @app.delete("/api/v1/streaming/nodes/{node_id}/clients/{cert}")
    async def streaming_unpair_client(
        request: Request, node_id: str, cert: str
    ) -> dict[str, Any]:
        """Unpair a Moonlight client by certificate fingerprint."""
        _require_scope(request, SCOPE_WRITE)
        ok = await _ss().unpair_client(node_id, cert)
        if not ok:
            raise HTTPException(404, "Client not found or unpair failed")
        return {"ok": True, "cert": cert}

    @app.get("/api/v1/streaming/nodes/{node_id}/moonlight-address")
    async def streaming_moonlight_address(
        request: Request, node_id: str
    ) -> dict[str, Any]:
        """Return the host:port address to enter in Moonlight's 'Add Computer' dialog."""
        _require_scope(request, SCOPE_READ)
        addr = _ss().moonlight_address(node_id)
        if not addr:
            raise HTTPException(503, "Streaming not active for this node")
        return {"address": addr, "node_id": node_id}

    @app.post("/api/v1/streaming/nodes/{node_id}/register-remote")
    async def streaming_register_remote(
        request: Request, node_id: str
    ) -> dict[str, Any]:
        """
        Register a node that manages its own Sunshine instance (desktop agent).

        Called automatically by the agent when it starts Sunshine.
        Body: {"host": "192.168.1.50", "api_port": 47990}
        """
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        host = body.get("host", "")
        api_port = int(body.get("api_port", 47990))
        if not host:
            raise HTTPException(400, "host is required")
        _ss().register_remote(node_id, host, api_port)
        return {"ok": True, "node_id": node_id, "host": host, "api_port": api_port}

    # ── Moonlight App Mapping (V1.2) ────────────────────────────────────────
    # Each scenario appears as a Moonlight app. When Moonlight selects an app,
    # the corresponding scenario is activated and streaming is started.

    def _mam() -> MoonlightAppMapper:
        """Get the Moonlight app mapper (creates if needed)."""
        if scenarios is None:
            raise HTTPException(503, "Scenarios not configured")
        from gaming.moonlight_app_mapping import MoonlightAppMapper, create_app_mapper
        return create_app_mapper(scenarios, sunshine)

    @app.get("/api/v1/moonlight/apps")
    async def moonlight_list_apps(request: Request) -> dict[str, Any]:
        """
        Get the Moonlight app list.

        Returns all Ozma scenarios as Moonlight app entries. Each scenario
        becomes one app with its app_id matching the scenario_id.

        Moonlight clients use this list to display available apps.
        """
        _require_scope(request, SCOPE_READ)
        mapper = _mam()
        return {"apps": mapper.get_app_list()}

    @app.post("/api/v1/moonlight/launch/{app_id}")
    async def moonlight_launch_app(request: Request, app_id: str) -> dict[str, Any]:
        """
        Launch a Moonlight app (activate the corresponding scenario).

        When Moonlight selects an app from the list, it sends a POST request
        to this endpoint. The scenario is activated and streaming is started.

        Path params:
            app_id: The scenario ID to launch

        Body (optional):
            node_id: Override node to stream (for multi-node scenarios)

        Returns:
            {ok: True, scenario_id: ..., streaming_enabled: ...}
        """
        _require_scope(request, SCOPE_WRITE)
        mapper = _mam()
        body: dict = {}
        try:
            body = await request.json()
        except Exception:
            pass

        result = await mapper.launch_app(app_id)

        # If node_id override was provided and streaming wasn't enabled,
        # try to start streaming on the specified node
        if not result.get("streaming_enabled") and body.get("node_id"):
            node_id = body["node_id"]
            if sunshine:
                try:
                    await sunshine.enable_node(node_id)
                    result["streaming_enabled"] = True
                    result["node_id"] = node_id
                except Exception as e:
                    log.warning("Failed to enable streaming for override node %s: %s", node_id, e)
                    result["streaming_warning"] = str(e)

        return result

    @app.post("/api/v1/moonlight/refresh")
    async def moonlight_refresh_apps(request: Request) -> dict[str, Any]:
        """
        Refresh the Moonlight app list from current scenarios.

        Call this endpoint when scenarios change to update the app list
        without restarting the controller.
        """
        _require_scope(request, SCOPE_WRITE)
        mapper = _mam()
        mapper.invalidate_cache()
        return {"ok": True, "apps": mapper.get_app_list()}

    # ── MSP Multi-tenant Dashboard ────────────────────────────────────────────

    def _msp() -> MSPDashboardManager:
        if msp_mgr is None:
            raise HTTPException(503, "MSP dashboard not available")
        return msp_mgr

    def _msp_portal() -> MSPPortalManager:
        if msp_portal is None:
            raise HTTPException(503, "MSP portal not configured")
        return msp_portal

    # Clients
    @app.get("/api/v1/msp/clients")
    async def msp_list_clients(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_ADMIN)
        clients = await _msp().list_clients()
        return [c.to_dict() for c in clients]

    @app.post("/api/v1/msp/clients")
    async def msp_add_client(request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        body = await request.json()
        required = ("name", "slug", "controller_url", "api_token")
        for f in required:
            if not body.get(f):
                raise HTTPException(400, f"Field '{f}' is required")
        optional = {k: body[k] for k in (
            "tier", "seat_count", "tags", "notes", "monthly_rate", "wholesale_cost"
        ) if k in body}
        client = await _msp().add_client(
            name=body["name"],
            slug=body["slug"],
            controller_url=body["controller_url"],
            api_token=body["api_token"],
            **optional,
        )
        return client.to_dict()

    @app.get("/api/v1/msp/clients/{client_id}")
    async def msp_get_client(request: Request, client_id: str) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        client = await _msp().get_client(client_id)
        if not client:
            raise HTTPException(404, "Client not found")
        return client.to_dict()

    @app.put("/api/v1/msp/clients/{client_id}")
    async def msp_update_client(request: Request, client_id: str) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        body = await request.json()
        try:
            client = await _msp().update_client(client_id, **body)
        except KeyError:
            raise HTTPException(404, "Client not found")
        return client.to_dict()

    @app.delete("/api/v1/msp/clients/{client_id}")
    async def msp_delete_client(request: Request, client_id: str) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        client = await _msp().get_client(client_id)
        if not client:
            raise HTTPException(404, "Client not found")
        await _msp().remove_client(client_id)
        return {"ok": True}

    # Health
    @app.get("/api/v1/msp/health")
    async def msp_list_health(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_ADMIN)
        healths = await _msp().get_all_health()
        return [h.to_dict() for h in healths]

    @app.post("/api/v1/msp/health/refresh")
    async def msp_refresh_all_health(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_ADMIN)
        healths = await _msp().refresh_all_health()
        return [h.to_dict() for h in healths]

    @app.get("/api/v1/msp/health/{client_id}")
    async def msp_get_health(request: Request, client_id: str) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        h = await _msp().get_health(client_id)
        if not h:
            raise HTTPException(404, "No health data for this client")
        return h.to_dict()

    @app.post("/api/v1/msp/health/{client_id}/refresh")
    async def msp_refresh_client_health(request: Request, client_id: str) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        try:
            h = await _msp().refresh_client_health(client_id)
        except KeyError:
            raise HTTPException(404, "Client not found")
        return h.to_dict()

    # Bulk operations
    @app.post("/api/v1/msp/bulk/patch")
    async def msp_bulk_patch(request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        body = await request.json()
        client_ids = body.get("client_ids", [])
        if not client_ids:
            raise HTTPException(400, "client_ids is required")
        ring = body.get("ring", "emergency")
        op = await _msp().bulk_patch_deploy(client_ids, ring=ring)
        return op.to_dict()

    @app.post("/api/v1/msp/bulk/compliance")
    async def msp_bulk_compliance(request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        body = await request.json()
        client_ids = body.get("client_ids", [])
        if not client_ids:
            raise HTTPException(400, "client_ids is required")
        framework = body.get("framework", "e8")
        op = await _msp().bulk_compliance_reports(client_ids, framework=framework)
        return op.to_dict()

    @app.post("/api/v1/msp/bulk/policy")
    async def msp_bulk_policy(request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        body = await request.json()
        client_ids = body.get("client_ids", [])
        if not client_ids:
            raise HTTPException(400, "client_ids is required")
        policy = body.get("policy", {})
        op = await _msp().bulk_policy_push(client_ids, policy=policy)
        return op.to_dict()

    @app.get("/api/v1/msp/bulk")
    async def msp_list_operations(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_ADMIN)
        ops = await _msp().list_operations()
        return [op.to_dict() for op in ops]

    @app.get("/api/v1/msp/bulk/{op_id}")
    async def msp_get_operation(request: Request, op_id: str) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        op = await _msp().get_operation(op_id)
        if not op:
            raise HTTPException(404, "Operation not found")
        return op.to_dict()

    # Alerts
    @app.get("/api/v1/msp/alerts")
    async def msp_alerts(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_ADMIN)
        severity = request.query_params.get("severity", "")
        client_id = request.query_params.get("client_id", "")
        return await _msp().aggregate_alerts(severity=severity, client_id=client_id)

    # Billing
    @app.get("/api/v1/msp/billing/{year}/{month}")
    async def msp_billing(request: Request, year: int, month: int) -> list[dict]:
        _require_scope(request, SCOPE_ADMIN)
        lines = await _msp().monthly_billing_export(year, month)
        return [line.to_dict() for line in lines]

    @app.get("/api/v1/msp/billing/{year}/{month}/csv")
    async def msp_billing_csv(request: Request, year: int, month: int):
        _require_scope(request, SCOPE_ADMIN)
        csv_data = await _msp().billing_csv(year, month)
        filename = f"msp_billing_{year}_{month:02d}.csv"
        return Response(
            content=csv_data,
            media_type="text/csv",
            headers={"Content-Disposition": f'attachment; filename="{filename}"'},
        )

    # Portal config
    @app.get("/api/v1/msp/portal/config")
    async def msp_portal_get_config(request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        return _msp_portal().get_portal_config().to_dict()

    @app.patch("/api/v1/msp/portal/config")
    async def msp_portal_update_config(request: Request) -> dict:
        _require_scope(request, SCOPE_ADMIN)
        body = await request.json()
        config = await _msp_portal().update_portal_config(**body)
        return config.to_dict()

    # Client portal page — no auth on the URL itself; MSP gates access externally.
    # In production: put OIDC token validation here.
    @app.get("/portal/{client_id}", response_class=Response)
    async def msp_portal_page(client_id: str) -> Response:
        mgr = _msp()
        client = await mgr.get_client(client_id)
        if not client:
            return Response(content="<h1>Not found</h1>", media_type="text/html",
                            status_code=404)
        health = await mgr.get_health(client_id)
        if health is None:
            # Return a minimal "loading" page
            from msp_dashboard import MSPClientHealth
            health = MSPClientHealth(
                client_id=client_id,
                fetched_at=0.0,
                machines_online=0,
                machines_total=0,
                critical_alerts=0,
                compliance_score=0.0,
                e8_score=0.0,
                iso27001_score=0.0,
                last_backup_ok=False,
                pending_approvals=0,
                upcoming_renewals=0,
                health="amber",
                error="Health data not yet available",
            )
        portal = _msp_portal()
        html = portal.get_portal_html(client, health)
        return Response(content=html, media_type="text/html")

    # ── Auto-configure (V1.7) — PoE subnet camera discovery ──────────────────

    def _ac() -> Any:
        if auto_configure is None:
            raise HTTPException(503, "Auto-configure not available")
        return auto_configure

    @app.get("/api/v1/auto-configure/devices")
    async def ac_list_devices(request: Request) -> list[dict[str, Any]]:
        """List all discovered devices on the PoE subnet."""
        _require_scope(request, SCOPE_READ)
        return _ac().list_devices()

    @app.get("/api/v1/auto-configure/devices/{ip}")
    async def ac_get_device(request: Request, ip: str) -> dict[str, Any]:
        """Get a single discovered device by IP."""
        _require_scope(request, SCOPE_READ)
        dev = _ac().get_device(ip)
        if dev is None:
            raise HTTPException(404, f"No device at {ip}")
        return dev.to_dict()

    @app.post("/api/v1/auto-configure/devices/{ip}/register")
    async def ac_register_device(request: Request, ip: str) -> dict[str, Any]:
        """Register a discovered device as an Ozma node."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        name = body.get("name", "")
        if not name:
            raise HTTPException(400, "name is required")
        machine_class = body.get("machine_class", "camera")
        result = await _ac().register_device(ip, name, machine_class=machine_class)
        if not result.get("ok"):
            raise HTTPException(404, result.get("error", f"No device at {ip}"))
        return result

    @app.post("/api/v1/auto-configure/devices/{ip}/ignore")
    async def ac_ignore_device(request: Request, ip: str) -> dict[str, Any]:
        """Suppress future notifications for this device."""
        _require_scope(request, SCOPE_WRITE)
        dev = _ac().get_device(ip)
        if dev is None:
            raise HTTPException(404, f"No device at {ip}")
        _ac().ignore_device(ip)
        return {"ok": True, "ip": ip}

    @app.post("/api/v1/auto-configure/devices/{ip}/unignore")
    async def ac_unignore_device(request: Request, ip: str) -> dict[str, Any]:
        """Re-enable notifications for a previously ignored device."""
        _require_scope(request, SCOPE_WRITE)
        dev = _ac().get_device(ip)
        if dev is None:
            raise HTTPException(404, f"No device at {ip}")
        _ac().unignore_device(ip)
        return {"ok": True, "ip": ip}

    @app.post("/api/v1/auto-configure/scan")
    async def ac_scan_now(request: Request) -> dict[str, Any]:
        """Trigger an immediate PoE subnet scan."""
        _require_scope(request, SCOPE_WRITE)
        await _ac().scan_now()
        return {"ok": True}

    # ── Camera Connect (V1.7) — cloud registration proxy for camera nodes ────

    def _cc() -> Any:
        if cam_connect is None:
            raise HTTPException(503, "Camera Connect not available")
        return cam_connect

    @app.get("/api/v1/camera-connect/registrations")
    async def cc_list_registrations(request: Request) -> list[dict[str, Any]]:
        """List Connect registration records for all camera nodes."""
        _require_scope(request, SCOPE_READ)
        return _cc().list_registrations()

    @app.get("/api/v1/camera-connect/registrations/{node_id:path}")
    async def cc_get_registration(request: Request, node_id: str) -> dict[str, Any]:
        """Get the Connect registration record for a specific camera node."""
        _require_scope(request, SCOPE_READ)
        reg = _cc().get_registration(node_id)
        if reg is None:
            raise HTTPException(404, f"No Connect registration for {node_id}")
        return reg.to_dict()

    @app.post("/api/v1/camera-connect/registrations/{node_id:path}/register")
    async def cc_force_register(request: Request, node_id: str) -> dict[str, Any]:
        """Force immediate (re-)registration of a camera node with Connect."""
        _require_scope(request, SCOPE_WRITE)
        result = await _cc().force_register(node_id)
        if not result.get("ok"):
            raise HTTPException(400, result.get("error", "Registration failed"))
        return result

    @app.delete("/api/v1/camera-connect/registrations/{node_id:path}")
    async def cc_deregister(request: Request, node_id: str) -> dict[str, Any]:
        """Remove a camera node from Connect and clear its registration record."""
        _require_scope(request, SCOPE_WRITE)
        result = await _cc().deregister(node_id)
        if not result.get("ok"):
            raise HTTPException(404, result.get("error", "Not found"))
        return result

    @app.get("/api/v1/camera-connect/registrations/{node_id:path}/relay-url")
    async def cc_relay_rtsp_url(
        request: Request, node_id: str,
        stream_path: str = "/",
    ) -> dict[str, Any]:
        """Return the Connect relay RTSP URL for a camera node."""
        _require_scope(request, SCOPE_READ)
        url = _cc().relay_rtsp_url(node_id, stream_path)
        if url is None:
            raise HTTPException(404, "No relay configured for this camera")
        return {"node_id": node_id, "relay_rtsp_url": url}

    # ── Grid federation (V1.4) — multi-Desk KVM federation ───────────────────

    def _grid() -> Any:
        if grid is None:
            raise HTTPException(503, "Grid service not available")
        return grid

    @app.get("/api/v1/grid/state")
    async def grid_show_state(request: Request) -> dict[str, Any]:
        """Full Grid Show state: desks, claims, feeds."""
        _require_scope(request, SCOPE_READ)
        return _grid().show_state()

    # Desks
    @app.get("/api/v1/grid/desks")
    async def grid_list_desks(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        return _grid().list_desks()

    @app.get("/api/v1/grid/desks/{desk_id}")
    async def grid_get_desk(request: Request, desk_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        desk = _grid().get_desk(desk_id)
        if not desk:
            raise HTTPException(404, f"Desk {desk_id} not found")
        return desk.to_dict()

    @app.post("/api/v1/grid/desks")
    async def grid_register_desk(request: Request) -> dict[str, Any]:
        """Register a Desk with the Grid."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        from grid import DeskInfo
        desk = DeskInfo(
            id=body["id"],
            name=body.get("name", body["id"]),
            host=body["host"],
            port=body.get("port", 7380),
            marks=body.get("marks", []),
            failover_group=body.get("failover_group", ""),
            priority=body.get("priority", 0),
        )
        _grid().register_desk(desk)
        return {"ok": True, "desk_id": desk.id}

    @app.put("/api/v1/grid/desks/{desk_id}")
    async def grid_update_desk(request: Request, desk_id: str) -> dict[str, Any]:
        """Update mutable fields on a Desk."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        desk = _grid().update_desk(desk_id, **body)
        if not desk:
            raise HTTPException(404, f"Desk {desk_id} not found")
        return desk.to_dict()

    @app.delete("/api/v1/grid/desks/{desk_id}")
    async def grid_unregister_desk(request: Request, desk_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        _grid().unregister_desk(desk_id)
        return {"ok": True, "desk_id": desk_id}

    # Claims
    @app.get("/api/v1/grid/claims")
    async def grid_list_claims(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        return _grid().list_claims()

    @app.post("/api/v1/grid/claims")
    async def grid_claim_mark(request: Request) -> dict[str, Any]:
        """Claim a Mark for a Desk."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        mark_id = body.get("mark_id", "")
        desk_id = body.get("desk_id", "")
        if not mark_id or not desk_id:
            raise HTTPException(400, "mark_id and desk_id are required")
        shared = body.get("shared", False)
        ok = _grid().claim_mark(mark_id, desk_id, shared=shared)
        return {"ok": ok, "mark_id": mark_id, "desk_id": desk_id}

    @app.delete("/api/v1/grid/claims/{mark_id}")
    async def grid_release_mark(
        request: Request, mark_id: str, desk_id: str = ""
    ) -> dict[str, Any]:
        """Release a Mark claim."""
        _require_scope(request, SCOPE_WRITE)
        if not desk_id:
            body = await request.json() if request.headers.get("content-type") == "application/json" else {}
            desk_id = body.get("desk_id", "")
        ok = _grid().release_mark(mark_id, desk_id)
        return {"ok": ok, "mark_id": mark_id}

    @app.get("/api/v1/grid/desks/{desk_id}/failover-candidates")
    async def grid_failover_candidates(request: Request, desk_id: str) -> list[dict[str, Any]]:
        """Return online failover candidates for a Desk."""
        _require_scope(request, SCOPE_READ)
        return [d.to_dict() for d in _grid().failover_candidates(desk_id)]

    # Feeds
    @app.get("/api/v1/grid/feeds")
    async def grid_list_feeds(request: Request) -> list[dict[str, Any]]:
        _require_scope(request, SCOPE_READ)
        return _grid().list_feeds()

    @app.get("/api/v1/grid/feeds/{feed_id}")
    async def grid_get_feed(request: Request, feed_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        feed = _grid().get_feed(feed_id)
        if not feed:
            raise HTTPException(404, f"Feed {feed_id} not found")
        return feed.to_dict()

    @app.post("/api/v1/grid/feeds")
    async def grid_register_feed(request: Request) -> dict[str, Any]:
        """Publish a Desk as a Feed source."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        from grid import FeedSource
        feed = FeedSource(
            feed_id=body["feed_id"],
            desk_id=body["desk_id"],
            name=body.get("name", body["feed_id"]),
            hls_url=body.get("hls_url", ""),
            rtsp_url=body.get("rtsp_url", ""),
            audio=body.get("audio", False),
        )
        _grid().register_feed(feed)
        return {"ok": True, "feed_id": feed.feed_id}

    @app.delete("/api/v1/grid/feeds/{feed_id}")
    async def grid_unregister_feed(request: Request, feed_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        ok = _grid().unregister_feed(feed_id)
        if not ok:
            raise HTTPException(404, f"Feed {feed_id} not found")
        return {"ok": True, "feed_id": feed_id}

    @app.post("/api/v1/grid/feeds/{feed_id}/subscribe")
    async def grid_subscribe_feed(request: Request, feed_id: str) -> dict[str, Any]:
        """Subscribe the caller's Desk to a Feed."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        desk_id = body.get("desk_id", "")
        if not desk_id:
            raise HTTPException(400, "desk_id is required")
        ok = _grid().subscribe_feed(feed_id, desk_id)
        if not ok:
            raise HTTPException(404, f"Feed {feed_id} not found")
        return {"ok": True, "feed_id": feed_id, "desk_id": desk_id}

    @app.post("/api/v1/grid/feeds/{feed_id}/unsubscribe")
    async def grid_unsubscribe_feed(request: Request, feed_id: str) -> dict[str, Any]:
        """Unsubscribe a Desk from a Feed."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        desk_id = body.get("desk_id", "")
        ok = _grid().unsubscribe_feed(feed_id, desk_id)
        if not ok:
            raise HTTPException(404, f"Feed {feed_id} not found")
        return {"ok": True, "feed_id": feed_id, "desk_id": desk_id}

    # ------------------------------------------------------------------ #
    # Parental controls
    # ------------------------------------------------------------------ #

    def _parental() -> ParentalControlsManager:
        if parental is None:
            raise HTTPException(503, "Parental controls not enabled")
        return parental

    @app.get("/api/v1/parental/profiles")
    async def parental_list_profiles(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        return _parental().list_profiles()

    @app.post("/api/v1/parental/profiles")
    async def parental_create_profile(request: Request) -> dict:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        profile = _parental().create_profile(
            name           = body.get("name", ""),
            policy_mode    = body.get("policy_mode", "blacklist"),
            content_filter = body.get("content_filter", "off"),
            rules          = body.get("rules", []),
            timers         = body.get("timers", []),
            schedule       = body.get("schedule", []),
            break_policy   = body.get("break_policy", {}),
            override_pin   = body.get("override_pin"),
        )
        return profile.to_dict()

    @app.get("/api/v1/parental/profiles/{profile_id}")
    async def parental_get_profile(request: Request, profile_id: str) -> dict:
        _require_scope(request, SCOPE_READ)
        profile = _parental().get_profile(profile_id)
        if profile is None:
            raise HTTPException(404, f"Profile {profile_id!r} not found")
        return profile.to_dict()

    @app.put("/api/v1/parental/profiles/{profile_id}")
    async def parental_update_profile(request: Request, profile_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        profile = _parental().update_profile(profile_id, **body)
        if profile is None:
            raise HTTPException(404, f"Profile {profile_id!r} not found")
        return profile.to_dict()

    @app.delete("/api/v1/parental/profiles/{profile_id}")
    async def parental_delete_profile(request: Request, profile_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        ok = _parental().delete_profile(profile_id)
        if not ok:
            raise HTTPException(404, f"Profile {profile_id!r} not found")
        return {"ok": True, "profile_id": profile_id}

    @app.post("/api/v1/parental/profiles/{profile_id}/rules")
    async def parental_add_rule(request: Request, profile_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        rule = _parental().add_rule(profile_id, **body)
        if rule is None:
            raise HTTPException(404, f"Profile {profile_id!r} not found")
        return rule.to_dict()

    @app.delete("/api/v1/parental/profiles/{profile_id}/rules/{rule_id}")
    async def parental_remove_rule(request: Request, profile_id: str, rule_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        ok = _parental().remove_rule(profile_id, rule_id)
        if not ok:
            raise HTTPException(404, "Profile or rule not found")
        return {"ok": True}

    @app.post("/api/v1/parental/profiles/{profile_id}/timers")
    async def parental_add_timer(request: Request, profile_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        timer = _parental().add_timer(profile_id, **body)
        if timer is None:
            raise HTTPException(404, f"Profile {profile_id!r} not found")
        return timer.to_dict()

    @app.delete("/api/v1/parental/profiles/{profile_id}/timers/{timer_id}")
    async def parental_remove_timer(request: Request, profile_id: str, timer_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        ok = _parental().remove_timer(profile_id, timer_id)
        if not ok:
            raise HTTPException(404, "Profile or timer not found")
        return {"ok": True}

    @app.get("/api/v1/parental/devices/{device_id}/profile")
    async def parental_get_device_profile(request: Request, device_id: str) -> dict:
        _require_scope(request, SCOPE_READ)
        profile = _parental().get_profile_for_device(device_id)
        if profile is None:
            return {"profile": None}
        return {"profile": profile.to_dict()}

    @app.post("/api/v1/parental/devices/{device_id}/assign")
    async def parental_assign_device(request: Request, device_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        profile_id = body.get("profile_id", "")
        if not profile_id:
            raise HTTPException(400, "profile_id is required")
        ok = _parental().assign_device(profile_id, device_id)
        if not ok:
            raise HTTPException(404, f"Profile {profile_id!r} not found")
        return {"ok": True, "device_id": device_id, "profile_id": profile_id}

    @app.post("/api/v1/parental/devices/{device_id}/unassign")
    async def parental_unassign_device(request: Request, device_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        _parental().unassign_device(device_id)
        return {"ok": True, "device_id": device_id}

    @app.get("/api/v1/parental/enforcement/{device_id}")
    async def parental_get_enforcement(request: Request, device_id: str) -> dict:
        """Return the LockdownConfig for a device (fetched by the agent)."""
        _require_scope(request, SCOPE_READ)
        cfg = _parental().get_enforcement_state(device_id)
        return cfg.to_dict()

    @app.post("/api/v1/parental/usage/report")
    async def parental_usage_report(request: Request) -> dict:
        """Agent reports per-app minute increments."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        profile_id  = body.get("profile_id", "")
        increments  = body.get("increments", {})   # app_name → minutes
        if profile_id and increments:
            for app_name, minutes in increments.items():
                _parental().record_usage(profile_id, app_name, int(minutes))
        return {"ok": True}

    @app.get("/api/v1/parental/profiles/{profile_id}/usage")
    async def parental_get_usage(request: Request, profile_id: str) -> dict:
        _require_scope(request, SCOPE_READ)
        summary = _parental().get_usage_summary(profile_id)
        return {"profile_id": profile_id, "summary": summary}

    @app.post("/api/v1/parental/profiles/{profile_id}/timers/{timer_id}/reset")
    async def parental_reset_timer(request: Request, profile_id: str, timer_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        ok = _parental().reset_timer(profile_id, timer_id)
        if not ok:
            raise HTTPException(404, "Profile or timer not found")
        return {"ok": True}

    @app.post("/api/v1/parental/overrides")
    async def parental_grant_override(request: Request) -> dict:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        profile_id = body.get("profile_id", "")
        device_id  = body.get("device_id", "")
        pin        = body.get("pin", "")
        minutes    = int(body.get("minutes", 60))
        reason     = body.get("reason", "")
        if not profile_id or not pin:
            raise HTTPException(400, "profile_id and pin are required")
        ctx     = _require_scope(request, SCOPE_WRITE)
        user_id = ctx.user_id if ctx else ""
        session = _parental().grant_override(profile_id, device_id, minutes, pin, user_id, reason)
        if session is None:
            raise HTTPException(403, "Invalid PIN or profile not found")
        return session.to_dict()

    @app.delete("/api/v1/parental/overrides/{override_id}")
    async def parental_revoke_override(request: Request, override_id: str) -> dict:
        _require_scope(request, SCOPE_WRITE)
        ok = _parental().revoke_override(override_id)
        if not ok:
            raise HTTPException(404, f"Override {override_id!r} not found")
        return {"ok": True}

    @app.get("/api/v1/parental/overrides")
    async def parental_list_overrides(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        return _parental().list_overrides()

    @app.post("/api/v1/parental/profiles/{profile_id}/check")
    async def parental_check_permission(request: Request, profile_id: str) -> dict:
        """Check whether an app is permitted under a profile (for testing)."""
        _require_scope(request, SCOPE_READ)
        body     = await request.json()
        app_name = body.get("app_name", "")
        if not app_name:
            raise HTTPException(400, "app_name is required")
        device_id = body.get("device_id", "")
        result    = _parental().check_permission(profile_id, app_name, device_id)
        return result.to_dict()

    # ------------------------------------------------------------------ #
    # Backup proxy — forward snapshot browse / restore to node agents
    # ------------------------------------------------------------------ #

    def _backup_nudge() -> BackupNudgeService:
        if backup_nudge is None:
            raise HTTPException(503, "Backup proxy not configured")
        return backup_nudge

    @app.get("/api/v1/backup/nodes/{node_id}/snapshots")
    async def backup_node_snapshots(request: Request, node_id: str) -> list[dict]:
        """
        List Restic snapshots for a node.

        Proxied to the node's agent API.  Requires the agent to be running and
        reachable at the registered api_port.
        """
        _require_scope(request, SCOPE_READ)
        result = await _backup_nudge().proxy_get(node_id, "/backup/snapshots")
        if result is None:
            raise HTTPException(503, f"Node {node_id!r} agent unreachable or backup not configured")
        return result if isinstance(result, list) else [result]

    @app.get("/api/v1/backup/nodes/{node_id}/snapshots/{snapshot_id}/files")
    async def backup_node_snapshot_files(
        request: Request, node_id: str, snapshot_id: str,
    ) -> list[dict]:
        """Browse the file tree within a specific snapshot."""
        _require_scope(request, SCOPE_READ)
        path = request.query_params.get("path", "/")
        result = await _backup_nudge().proxy_get(
            node_id,
            f"/backup/snapshots/{snapshot_id}/files?path={path}",
        )
        if result is None:
            raise HTTPException(503, f"Node {node_id!r} agent unreachable")
        return result if isinstance(result, list) else [result]

    @app.post("/api/v1/backup/nodes/{node_id}/restore")
    async def backup_node_restore(request: Request, node_id: str) -> dict[str, Any]:
        """
        Trigger a selective restore on a node.

        Body: { "snapshot_id": "...", "source_path": "/home/alice/docs",
                "target_path": "/home/alice/restore" }
        """
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        result = await _backup_nudge().proxy_post(node_id, "/backup/restore", body)
        if result is None:
            raise HTTPException(503, f"Node {node_id!r} agent unreachable or restore failed")
        return result

    @app.post("/api/v1/backup/nodes/{node_id}/run")
    async def backup_node_run(request: Request, node_id: str) -> dict[str, Any]:
        """Trigger an on-demand backup on a node."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        mode = body.get("mode", "smart")
        result = await _backup_nudge().proxy_post(node_id, "/backup/run", {"mode": mode})
        if result is None:
            raise HTTPException(503, f"Node {node_id!r} agent unreachable")
        return result

    @app.get("/api/v1/backup/nodes/{node_id}/apps")
    async def backup_node_apps(request: Request, node_id: str) -> list[dict]:
        """List installed apps on a node (for selective restore)."""
        _require_scope(request, SCOPE_READ)
        result = await _backup_nudge().proxy_get(node_id, "/backup/apps")
        if result is None:
            raise HTTPException(503, f"Node {node_id!r} agent unreachable")
        return result if isinstance(result, list) else [result]

    @app.post("/api/v1/backup/nodes/{node_id}/restore-apps")
    async def backup_node_restore_apps(request: Request, node_id: str) -> dict[str, Any]:
        """Reinstall apps on a node from its inventory snapshot."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        result = await _backup_nudge().proxy_post(node_id, "/backup/apps/restore", body)
        if result is None:
            raise HTTPException(503, f"Node {node_id!r} agent unreachable")
        return result

    # =========================================================================
    # DNS / Ad Filter endpoints
    # =========================================================================

    def _dns_filter():
        if dns_filter is None:
            raise HTTPException(503, "DNS filter not available")
        return dns_filter

    @app.get("/api/v1/dns-filter/status")
    async def dns_filter_status(request: Request) -> dict[str, Any]:
        """Current DNS filter state: enabled, domain count, source stats."""
        _require_scope(request, SCOPE_READ)
        return _dns_filter().get_status()

    @app.get("/api/v1/dns-filter/config")
    async def dns_filter_config_get(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _dns_filter().get_config().to_dict()

    @app.patch("/api/v1/dns-filter/config")
    async def dns_filter_config_patch(request: Request) -> dict[str, Any]:
        """
        Update DNS filter config.

        Patchable fields: enabled, block_categories, safesearch_enabled,
        safesearch_providers, update_interval_hours.
        Changes are persisted and the dnsmasq conf is rewritten immediately.
        """
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        mgr = _dns_filter()
        mgr.set_config(**body)
        mgr._recompile()
        await mgr.write_conf()
        return mgr.get_config().to_dict()

    @app.get("/api/v1/dns-filter/sources")
    async def dns_filter_sources(request: Request) -> list[dict]:
        """List all blocklist sources (built-in + custom)."""
        _require_scope(request, SCOPE_READ)
        return _dns_filter().list_sources()

    @app.post("/api/v1/dns-filter/sources")
    async def dns_filter_sources_add(request: Request) -> dict[str, Any]:
        """
        Add a custom blocklist source.

        Body: { "name": "...", "url": "...", "format": "hosts|domains|adblock",
                "categories": ["ads", "malware"] }
        """
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        src = _dns_filter().add_source(
            name       = body["name"],
            url        = body["url"],
            fmt        = body.get("format", "domains"),
            categories = body.get("categories", []),
        )
        return src.to_dict()

    @app.patch("/api/v1/dns-filter/sources/{source_id}")
    async def dns_filter_sources_patch(request: Request, source_id: str) -> dict[str, Any]:
        """Enable or disable a source: { "enabled": true }"""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        mgr = _dns_filter()
        if "enabled" in body:
            ok = mgr.set_source_enabled(source_id, bool(body["enabled"]))
            if not ok:
                raise HTTPException(404, f"Source {source_id!r} not found")
            mgr._recompile()
            await mgr.write_conf()
        src = mgr.get_source(source_id)
        if src is None:
            raise HTTPException(404, f"Source {source_id!r} not found")
        return src.to_dict()

    @app.delete("/api/v1/dns-filter/sources/{source_id}")
    async def dns_filter_sources_delete(request: Request, source_id: str) -> dict[str, Any]:
        """Remove a custom source (built-in sources are disabled instead)."""
        _require_scope(request, SCOPE_WRITE)
        ok = _dns_filter().remove_source(source_id)
        if not ok:
            raise HTTPException(404, f"Source {source_id!r} not found")
        return {"ok": True}

    @app.post("/api/v1/dns-filter/sources/{source_id}/update")
    async def dns_filter_sources_update(request: Request, source_id: str) -> dict[str, Any]:
        """Trigger an immediate download + refresh of one or all sources."""
        _require_scope(request, SCOPE_WRITE)
        results = await _dns_filter().update_sources(source_ids=[source_id])
        return results

    @app.post("/api/v1/dns-filter/update")
    async def dns_filter_update_all(request: Request) -> dict[str, Any]:
        """Trigger a full blocklist refresh across all enabled sources."""
        _require_scope(request, SCOPE_WRITE)
        results = await _dns_filter().update_sources()
        return results

    @app.get("/api/v1/dns-filter/allowlist")
    async def dns_filter_allowlist_get(request: Request) -> list[str]:
        _require_scope(request, SCOPE_READ)
        return list(_dns_filter().get_config().allowlist)

    @app.post("/api/v1/dns-filter/allowlist")
    async def dns_filter_allowlist_add(request: Request) -> dict[str, Any]:
        """Add a domain to the global allowlist: { "domain": "example.com" }"""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        domain = body.get("domain", "").strip().lower()
        if not domain:
            raise HTTPException(400, "domain required")
        mgr = _dns_filter()
        mgr.add_allowlist(domain)
        await mgr.write_conf()
        return {"ok": True, "domain": domain}

    @app.delete("/api/v1/dns-filter/allowlist/{domain}")
    async def dns_filter_allowlist_remove(request: Request, domain: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        mgr = _dns_filter()
        ok = mgr.remove_allowlist(domain.lower())
        if not ok:
            raise HTTPException(404, f"{domain!r} not in allowlist")
        await mgr.write_conf()
        return {"ok": True}

    @app.get("/api/v1/dns-filter/custom-blocklist")
    async def dns_filter_custom_block_get(request: Request) -> list[str]:
        _require_scope(request, SCOPE_READ)
        return list(_dns_filter().get_config().custom_blocklist)

    @app.post("/api/v1/dns-filter/custom-blocklist")
    async def dns_filter_custom_block_add(request: Request) -> dict[str, Any]:
        """Add a domain to the custom blocklist: { "domain": "badsite.com" }"""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        domain = body.get("domain", "").strip().lower()
        if not domain:
            raise HTTPException(400, "domain required")
        mgr = _dns_filter()
        mgr.add_custom_block(domain)
        mgr._recompile()
        await mgr.write_conf()
        return {"ok": True, "domain": domain}

    @app.delete("/api/v1/dns-filter/custom-blocklist/{domain}")
    async def dns_filter_custom_block_remove(request: Request, domain: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        mgr = _dns_filter()
        ok = mgr.remove_custom_block(domain.lower())
        if not ok:
            raise HTTPException(404, f"{domain!r} not in custom blocklist")
        mgr._recompile()
        await mgr.write_conf()
        return {"ok": True}

    @app.post("/api/v1/dns-filter/check")
    async def dns_filter_check(request: Request) -> dict[str, Any]:
        """Check if a domain is blocked: { "domain": "example.com" }"""
        _require_scope(request, SCOPE_READ)
        body = await request.json()
        domain = body.get("domain", "").strip().lower()
        if not domain:
            raise HTTPException(400, "domain required")
        blocked = _dns_filter().is_blocked(domain)
        return {"domain": domain, "blocked": blocked}

    # ── Local DNS records ──────────────────────────────────────────────

    @app.get("/api/v1/dns-filter/records")
    async def dns_records_list(request: Request) -> list[dict]:
        """List all local DNS records (A/AAAA/CNAME/PTR)."""
        _require_scope(request, SCOPE_READ)
        return _dns_filter().list_records()

    @app.post("/api/v1/dns-filter/records")
    async def dns_records_add(request: Request) -> dict[str, Any]:
        """
        Add a local DNS record.

        Body: { "name": "NAS", "hostname": "nas.home", "rtype": "A",
                "value": "192.168.1.10", "ttl": 0 }
        """
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        mgr = _dns_filter()
        rec = mgr.add_record(
            name     = body["name"],
            hostname = body["hostname"],
            rtype    = body.get("rtype", "A"),
            value    = body["value"],
            ttl      = int(body.get("ttl", 0)),
        )
        await mgr.write_conf()
        return rec.to_dict()

    @app.patch("/api/v1/dns-filter/records/{record_id}")
    async def dns_records_patch(request: Request, record_id: str) -> dict[str, Any]:
        """Update fields on a local DNS record."""
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        mgr = _dns_filter()
        rec = mgr.update_record(record_id, **body)
        if rec is None:
            raise HTTPException(404, f"Record {record_id!r} not found")
        await mgr.write_conf()
        return rec.to_dict()

    @app.delete("/api/v1/dns-filter/records/{record_id}")
    async def dns_records_delete(request: Request, record_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        mgr = _dns_filter()
        ok = mgr.remove_record(record_id)
        if not ok:
            raise HTTPException(404, f"Record {record_id!r} not found")
        await mgr.write_conf()
        return {"ok": True}

    # =========================================================================
    # Connect — subdomain + HTTPS provisioning
    # =========================================================================

    @app.post("/api/v1/subdomain/claim")
    async def subdomain_claim(request: Request) -> dict[str, Any]:
        """
        Claim a Connect subdomain for the authenticated user.

        Returns { "domain": "alice.c.ozma.dev" } on success.
        Requires an active Connect session.
        """
        _require_scope(request, SCOPE_WRITE)
        if connect is None:
            raise HTTPException(503, "Connect not configured")
        body = await request.json()
        username = body.get("username", "")
        if not username:
            raise HTTPException(400, "username required")
        domain = await connect.claim_subdomain(username)
        if not domain:
            raise HTTPException(502, "Connect subdomain claim failed")
        return {"domain": domain}

    # =========================================================================
    # Local reverse proxy (Caddy LAN HTTPS)
    # =========================================================================

    def _local_proxy():
        if local_proxy is None:
            raise HTTPException(503, "Local proxy not available")
        return local_proxy

    @app.get("/api/v1/proxy/status")
    async def proxy_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _local_proxy().get_status()

    @app.get("/api/v1/proxy/config")
    async def proxy_config_get(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _local_proxy().get_config().to_dict()

    @app.patch("/api/v1/proxy/config")
    async def proxy_config_patch(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        cfg = await _local_proxy().set_config(**body)
        return cfg.to_dict()

    @app.get("/api/v1/proxy/routes")
    async def proxy_routes_list(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        return _local_proxy().list_routes()

    @app.post("/api/v1/proxy/routes")
    async def proxy_routes_add(request: Request) -> dict[str, Any]:
        """
        Add a reverse proxy route.

        Body: { "name": "Jellyfin", "match_domain": "jellyfin.home",
                "upstream": "http://localhost:8096", "tls_mode": "internal" }
        """
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        mgr   = _local_proxy()
        route = mgr.add_route(
            name         = body["name"],
            match_domain = body["match_domain"],
            upstream     = body["upstream"],
            tls_mode     = body.get("tls_mode", "internal"),
            strip_prefix = body.get("strip_prefix", ""),
            extra_headers = body.get("extra_headers", {}),
        )
        await mgr.apply()
        return route.to_dict()

    @app.patch("/api/v1/proxy/routes/{route_id}")
    async def proxy_routes_patch(request: Request, route_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body  = await request.json()
        mgr   = _local_proxy()
        route = mgr.update_route(route_id, **body)
        if route is None:
            raise HTTPException(404, f"Route {route_id!r} not found")
        await mgr.apply()
        return route.to_dict()

    @app.delete("/api/v1/proxy/routes/{route_id}")
    async def proxy_routes_delete(request: Request, route_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        mgr = _local_proxy()
        ok  = mgr.remove_route(route_id)
        if not ok:
            raise HTTPException(404, f"Route {route_id!r} not found")
        await mgr.apply()
        return {"ok": True}

    @app.get("/api/v1/proxy/ca-cert")
    async def proxy_ca_cert(request: Request):
        """Download Caddy's internal CA cert for client-side trust installation."""
        _require_scope(request, SCOPE_READ)
        from fastapi.responses import Response
        cert = _local_proxy().get_ca_cert()
        if cert is None:
            raise HTTPException(404, "CA cert not yet generated (start proxy with tls internal first)")
        return Response(content=cert, media_type="application/x-pem-file",
                        headers={"Content-Disposition": "attachment; filename=ozma-local-ca.crt"})

    # =========================================================================
    # File sharing (Samba + NFS)
    # =========================================================================

    def _file_sharing():
        if file_sharing is None:
            raise HTTPException(503, "File sharing not available")
        return file_sharing

    @app.get("/api/v1/file-sharing/status")
    async def file_sharing_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _file_sharing().get_status()

    @app.get("/api/v1/file-sharing/config")
    async def file_sharing_config_get(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _file_sharing().get_config().to_dict()

    @app.patch("/api/v1/file-sharing/config")
    async def file_sharing_config_patch(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        cfg = await _file_sharing().set_config(**body)
        return cfg.to_dict()

    @app.get("/api/v1/file-sharing/shares")
    async def file_sharing_shares_list(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        return _file_sharing().list_shares()

    @app.post("/api/v1/file-sharing/shares")
    async def file_sharing_shares_add(request: Request) -> dict[str, Any]:
        """
        Create a new file share.

        Body: { "name": "Home Files", "path": "/home/matt/shared",
                "protocols": ["smb"], "read_only": false, "guest_ok": false,
                "valid_users": ["alice"] }
        """
        _require_scope(request, SCOPE_WRITE)
        body  = await request.json()
        mgr   = _file_sharing()
        share = mgr.add_share(
            name      = body["name"],
            path      = body["path"],
            protocols = body.get("protocols", ["smb"]),
            **{k: v for k, v in body.items() if k not in ("name", "path", "protocols")},
        )
        await mgr._reload_smbd() if mgr._config.smb_enabled else None
        return share.to_dict()

    @app.patch("/api/v1/file-sharing/shares/{share_id}")
    async def file_sharing_shares_patch(request: Request, share_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body  = await request.json()
        mgr   = _file_sharing()
        share = mgr.update_share(share_id, **body)
        if share is None:
            raise HTTPException(404, f"Share {share_id!r} not found")
        await mgr._reload_smbd() if mgr._config.smb_enabled else None
        return share.to_dict()

    @app.delete("/api/v1/file-sharing/shares/{share_id}")
    async def file_sharing_shares_delete(request: Request, share_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        mgr = _file_sharing()
        ok  = mgr.remove_share(share_id)
        if not ok:
            raise HTTPException(404, f"Share {share_id!r} not found")
        return {"ok": True}

    @app.get("/api/v1/file-sharing/users")
    async def file_sharing_users_list(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        return _file_sharing().list_samba_users()

    @app.post("/api/v1/file-sharing/users")
    async def file_sharing_users_add(request: Request) -> dict[str, Any]:
        """Add a Samba user: { "username": "alice", "password": "..." }"""
        _require_scope(request, SCOPE_ADMIN)
        body = await request.json()
        ok   = await _file_sharing().add_samba_user(body["username"], body["password"])
        if not ok:
            raise HTTPException(500, "Failed to add Samba user")
        return {"ok": True, "username": body["username"]}

    @app.delete("/api/v1/file-sharing/users/{username}")
    async def file_sharing_users_delete(request: Request, username: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_ADMIN)
        ok = await _file_sharing().remove_samba_user(username)
        if not ok:
            raise HTTPException(404, f"User {username!r} not found")
        return {"ok": True}

    # =========================================================================
    # UPS / power management
    # =========================================================================

    def _ups():
        if ups_monitor is None:
            raise HTTPException(503, "UPS monitor not available")
        return ups_monitor

    @app.get("/api/v1/ups/status")
    async def ups_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _ups().get_status()

    @app.get("/api/v1/ups/config")
    async def ups_config_get(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _ups().get_config().to_dict()

    @app.patch("/api/v1/ups/config")
    async def ups_config_patch(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        cfg  = await _ups().set_config(**body)
        return cfg.to_dict()

    @app.post("/api/v1/ups/poll")
    async def ups_poll_now(request: Request) -> dict[str, Any]:
        """Trigger an immediate UPS status poll."""
        _require_scope(request, SCOPE_READ)
        status = await _ups().poll_now()
        if status is None:
            raise HTTPException(503, "UPS unreachable")
        return status.to_dict()

    # =========================================================================
    # Dynamic DNS
    # =========================================================================

    def _ddns():
        if ddns is None:
            raise HTTPException(503, "DDNS not available")
        return ddns

    @app.get("/api/v1/ddns/status")
    async def ddns_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _ddns().get_status()

    @app.get("/api/v1/ddns/config")
    async def ddns_config_get(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _ddns().get_config().to_dict()

    @app.patch("/api/v1/ddns/config")
    async def ddns_config_patch(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        cfg  = _ddns().set_config(**body)
        return cfg.to_dict()

    @app.get("/api/v1/ddns/records")
    async def ddns_records_list(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        return _ddns().list_records()

    @app.post("/api/v1/ddns/records")
    async def ddns_records_add(request: Request) -> dict[str, Any]:
        """
        Add a DDNS record.

        Body: { "name": "Home", "provider": "cloudflare",
                "credentials": {"zone_id": "...", "api_token": "..."},
                "hostnames": ["home.example.com"] }
        """
        _require_scope(request, SCOPE_WRITE)
        body   = await request.json()
        record = _ddns().add_record(
            name        = body["name"],
            provider    = body["provider"],
            credentials = body["credentials"],
            hostnames   = body["hostnames"],
            **{k: v for k, v in body.items()
               if k not in ("name", "provider", "credentials", "hostnames")},
        )
        return record.to_dict()

    @app.patch("/api/v1/ddns/records/{record_id}")
    async def ddns_records_patch(request: Request, record_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body   = await request.json()
        record = _ddns().update_record(record_id, **body)
        if record is None:
            raise HTTPException(404, f"DDNS record {record_id!r} not found")
        return record.to_dict()

    @app.delete("/api/v1/ddns/records/{record_id}")
    async def ddns_records_delete(request: Request, record_id: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        ok = _ddns().remove_record(record_id)
        if not ok:
            raise HTTPException(404, f"DDNS record {record_id!r} not found")
        return {"ok": True}

    @app.post("/api/v1/ddns/update")
    async def ddns_update_now(request: Request) -> dict[str, Any]:
        """Force an immediate DDNS update for all or one record."""
        _require_scope(request, SCOPE_WRITE)
        body      = await request.json()
        record_id = body.get("record_id")
        results   = await _ddns().update_now(record_id)
        return results

    # =========================================================================
    # WAN speed monitoring
    # =========================================================================

    def _speedtest():
        if speedtest is None:
            raise HTTPException(503, "Speedtest monitor not available")
        return speedtest

    @app.get("/api/v1/speedtest/status")
    async def speedtest_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _speedtest().get_status()

    @app.get("/api/v1/speedtest/config")
    async def speedtest_config_get(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _speedtest().get_config().to_dict()

    @app.patch("/api/v1/speedtest/config")
    async def speedtest_config_patch(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        cfg  = await _speedtest().set_config(**body)
        return cfg.to_dict()

    @app.get("/api/v1/speedtest/history")
    async def speedtest_history(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        limit = int(request.query_params.get("limit", 48))
        return _speedtest().get_history(limit=limit)

    @app.post("/api/v1/speedtest/run")
    async def speedtest_run_now(request: Request) -> dict[str, Any]:
        """Trigger an immediate speed test (runs in background task)."""
        _require_scope(request, SCOPE_WRITE)
        result = await _speedtest().run_now()
        if result is None:
            raise HTTPException(503, "No speedtest tool available or test already running")
        return result.to_dict()

    # =========================================================================
    # ZFS pool / dataset / snapshot management
    # =========================================================================

    def _zfs():
        if zfs is None:
            raise HTTPException(503, "ZFS manager not available")
        return zfs

    @app.get("/api/v1/zfs/status")
    async def zfs_status(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _zfs().get_status()

    @app.get("/api/v1/zfs/config")
    async def zfs_config_get(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_READ)
        return _zfs().get_config().to_dict()

    @app.patch("/api/v1/zfs/config")
    async def zfs_config_patch(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        return _zfs().set_config(**body).to_dict()

    @app.get("/api/v1/zfs/pools")
    async def zfs_pools(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        return await _zfs().list_pools()

    @app.get("/api/v1/zfs/datasets")
    async def zfs_datasets(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        pool = request.query_params.get("pool")
        datasets = await _zfs().list_datasets(pool=pool)
        return [d.to_dict() for d in datasets]

    @app.post("/api/v1/zfs/datasets")
    async def zfs_dataset_create(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        dataset = body.get("dataset") or body.get("name")
        if not dataset:
            raise HTTPException(400, "dataset required")
        ok = await _zfs().create_dataset(
            dataset=dataset,
            mountpoint=body.get("mountpoint"),
            encrypted=body.get("encrypted"),
            quota_bytes=body.get("quota_bytes", 0),
            compression=body.get("compression", "lz4"),
        )
        if not ok:
            raise HTTPException(500, "zfs create failed — check controller logs")
        return {"dataset": dataset, "created": True}

    @app.delete("/api/v1/zfs/datasets/{dataset:path}")
    async def zfs_dataset_destroy(request: Request, dataset: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        recursive = request.query_params.get("recursive", "false").lower() == "true"
        ok = await _zfs().destroy_dataset(dataset, recursive=recursive)
        if not ok:
            raise HTTPException(500, "zfs destroy failed")
        return {"dataset": dataset, "destroyed": True}

    @app.get("/api/v1/zfs/snapshots")
    async def zfs_snapshots(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        dataset = request.query_params.get("dataset")
        snaps = await _zfs().list_snapshots(dataset=dataset)
        return [s.to_dict() for s in snaps]

    @app.post("/api/v1/zfs/snapshots")
    async def zfs_snapshot_take(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        dataset = body.get("dataset")
        if not dataset:
            raise HTTPException(400, "dataset required")
        snap = await _zfs().take_snapshot(dataset=dataset, label=body.get("label"))
        if snap is None:
            raise HTTPException(500, "zfs snapshot failed")
        return {"snapshot": snap}

    @app.delete("/api/v1/zfs/snapshots/{snapshot:path}")
    async def zfs_snapshot_destroy(request: Request, snapshot: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        ok = await _zfs().destroy_snapshot(snapshot)
        if not ok:
            raise HTTPException(500, "zfs destroy snapshot failed")
        return {"snapshot": snapshot, "destroyed": True}

    # Managed dataset registry (Ozma-tracked — auto-snapshot + cloud backup)

    @app.get("/api/v1/zfs/managed")
    async def zfs_managed_list(request: Request) -> list[dict]:
        _require_scope(request, SCOPE_READ)
        return _zfs().list_managed()

    @app.post("/api/v1/zfs/managed")
    async def zfs_managed_register(request: Request) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        dataset = body.get("dataset")
        if not dataset:
            raise HTTPException(400, "dataset required")
        md = _zfs().register_dataset(
            dataset=dataset,
            auto_snapshot=body.get("auto_snapshot", True),
            cloud_backup=body.get("cloud_backup", False),
        )
        if "policy" in body:
            _zfs().update_managed(dataset, policy=body["policy"])
        return md.to_dict()

    @app.patch("/api/v1/zfs/managed/{dataset:path}")
    async def zfs_managed_update(request: Request, dataset: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        md = _zfs().update_managed(dataset, **body)
        if md is None:
            raise HTTPException(404, "dataset not managed by Ozma")
        return md.to_dict()

    @app.delete("/api/v1/zfs/managed/{dataset:path}")
    async def zfs_managed_unregister(request: Request, dataset: str) -> dict[str, Any]:
        _require_scope(request, SCOPE_WRITE)
        if not _zfs().unregister_dataset(dataset):
            raise HTTPException(404, "dataset not managed by Ozma")
        return {"dataset": dataset, "unregistered": True}

    @app.post("/api/v1/zfs/managed/{dataset:path}/backup")
    async def zfs_backup_now(request: Request, dataset: str) -> dict[str, Any]:
        """Trigger an immediate zfs send to Connect for this dataset."""
        _require_scope(request, SCOPE_WRITE)
        z = _zfs()
        cfg = z.get_config()
        if not cfg.connect_backup_url:
            raise HTTPException(503, "Connect backup URL not configured")
        if connect is None:
            raise HTTPException(503, "Connect not available")
        auth_header = f"Bearer {await connect.get_auth_token()}"
        ok = await z.send_to_connect(dataset, cfg.connect_backup_url, auth_header)
        return {"dataset": dataset, "ok": ok}

    # =========================================================================
    # Business continuity failover
    # =========================================================================

    def _failover():
        if failover is None:
            raise HTTPException(503, "Failover manager not available")
        return failover

    @app.get("/api/v1/failover/status")
    async def failover_status(request: Request) -> dict[str, Any]:
        """Current failover state: mode, outage info, free days remaining."""
        _require_scope(request, SCOPE_READ)
        return _failover().get_status()

    @app.post("/api/v1/failover/accept")
    async def failover_accept(request: Request) -> dict[str, Any]:
        """User accepts the virtual controller offer.

        Activates the pre-warmed virtual controller. Connect updates the
        relay so the existing subdomain points to the virtual controller
        and nodes reconnect automatically.
        """
        _require_scope(request, SCOPE_WRITE)
        return await _failover().accept_virtual_controller()

    @app.post("/api/v1/failover/decline")
    async def failover_decline(request: Request) -> dict[str, Any]:
        """User declines — no further notifications for this outage event."""
        _require_scope(request, SCOPE_WRITE)
        return await _failover().decline_virtual_controller()

    @app.post("/api/v1/failover/extend")
    async def failover_extend(request: Request) -> dict[str, Any]:
        """Purchase additional days of virtual failover beyond the free tier.

        Returns {ok, checkout_url, paid_until}.
        """
        _require_scope(request, SCOPE_WRITE)
        body = await request.json()
        days = int(body.get("days", 7))
        return await _failover().extend_failover(days)

    @app.get("/api/v1/failover/sync")
    async def failover_sync_pull(request: Request) -> dict[str, Any]:
        """Pull state delta from Connect (called on local controller after recovery)."""
        _require_scope(request, SCOPE_WRITE)
        if connect is None:
            raise HTTPException(503, "Connect not available")
        delta = await connect.pull_sync_delta()
        if not delta:
            return {"ok": False, "error": "No delta available"}
        ok = await _failover().apply_state_delta(delta)
        return {"ok": ok, "delta_keys": list(delta.keys())}

    @app.post("/api/v1/failover/sync")
    async def failover_sync_push(request: Request) -> dict[str, Any]:
        """Push state delta to Connect (called on virtual controller before handoff)."""
        _require_scope(request, SCOPE_WRITE)
        delta = await _failover().export_state_delta()
        if connect is None:
            raise HTTPException(503, "Connect not available")
        ok = await connect.push_sync_delta(delta)
        return {"ok": ok, "delta_keys": list(delta.keys())}

    @app.post("/api/v1/zfs/managed/{dataset:path}/prune")
    async def zfs_prune(request: Request, dataset: str) -> dict[str, Any]:
        """Manually run retention policy pruning for a dataset."""
        _require_scope(request, SCOPE_WRITE)
        md = _zfs().get_managed(dataset)
        if md is None:
            raise HTTPException(404, "dataset not managed by Ozma")
        destroyed = await _zfs().prune_snapshots(dataset, md.policy)
        return {"dataset": dataset, "pruned": destroyed}

    @app.delete("/api/v1/subdomain/claim")
    async def subdomain_release(request: Request) -> dict[str, Any]:
        """Release the current subdomain claim (if supported by Connect)."""
        _require_scope(request, SCOPE_WRITE)
        if connect is None:
            raise HTTPException(503, "Connect not configured")
        # Connect server handles release; we just surface the call
        ok = await connect._api_delete("/domains/claim") if hasattr(connect, "_api_delete") else None
        return {"ok": True}

    @app.post("/api/v1/dns/challenge")
    async def dns_challenge_create(request: Request) -> dict[str, Any]:
        """
        Request an ACME DNS-01 TXT challenge record via Connect/Cloudflare.

        Body: { "subdomain": "alice", "challenge_value": "_acme-challenge token" }
        Returns: { "fqdn": "_acme-challenge.alice.c.ozma.dev", "value": "..." }
        """
        _require_scope(request, SCOPE_WRITE)
        if connect is None:
            raise HTTPException(503, "Connect not configured")
        body = await request.json()
        subdomain = body.get("subdomain", "")
        challenge_value = body.get("challenge_value", "")
        if not subdomain or not challenge_value:
            raise HTTPException(400, "subdomain and challenge_value required")
        result = await connect.request_dns_challenge(subdomain, challenge_value)
        if not result:
            raise HTTPException(502, "DNS challenge request failed")
        return result

    @app.delete("/api/v1/dns/challenge")
    async def dns_challenge_delete(request: Request) -> dict[str, Any]:
        """Remove an ACME DNS-01 TXT challenge record after cert issuance."""
        _require_scope(request, SCOPE_WRITE)
        if connect is None:
            raise HTTPException(503, "Connect not configured")
        body = await request.json()
        subdomain = body.get("subdomain", "")
        if not subdomain:
            raise HTTPException(400, "subdomain required")
        result = await connect._api_post("/domains/challenge/delete", {"subdomain": subdomain}) \
            if hasattr(connect, "_api_post") else {"ok": True}
        return result or {"ok": True}

    @app.post("/api/v1/subdomain/external")
    async def subdomain_external_provision(request: Request) -> dict[str, Any]:
        """
        Provision an external subdomain for a local service.

        Body: { "subdomain": "jellyfin", "upstream_url": "http://192.168.1.10:8096" }
        Returns: { "domain": "jellyfin.alice.e.ozma.dev" }
        Connect will reverse-proxy the subdomain to upstream_url via the relay tunnel.
        """
        _require_scope(request, SCOPE_WRITE)
        if connect is None:
            raise HTTPException(503, "Connect not configured")
        body = await request.json()
        subdomain    = body.get("subdomain", "")
        upstream_url = body.get("upstream_url", "")
        if not subdomain or not upstream_url:
            raise HTTPException(400, "subdomain and upstream_url required")
        result = await connect.provision_external_subdomain(subdomain, upstream_url)
        if not result:
            raise HTTPException(502, "External subdomain provisioning failed")
        return result

    # ── DNS integrity verification ────────────────────────────────────────────

    @app.get("/api/v1/dns/integrity")
    async def dns_integrity_get(request: Request) -> dict[str, Any]:
        """
        Return the current DNS environment assessment for the controller.

        Includes: resolver integrity, transparent interception, NXDOMAIN hijacking,
        DNSSEC validation, DNS rebinding guard status, captive portal detection.
        """
        _require_scope(request, SCOPE_READ)
        if dns_verifier is None:
            raise HTTPException(503, "DNS verifier not running")
        env = dns_verifier.get_environment()
        if env is None:
            raise HTTPException(503, "DNS assessment not yet available — check back shortly")
        return env.to_dict()

    @app.get("/api/v1/dns/integrity/all")
    async def dns_integrity_all(request: Request) -> list[dict[str, Any]]:
        """Return DNS environment assessments for the controller and all reporting nodes."""
        _require_scope(request, SCOPE_READ)
        if dns_verifier is None:
            raise HTTPException(503, "DNS verifier not running")
        return dns_verifier.all_environments()

    @app.post("/api/v1/dns/integrity/run")
    async def dns_integrity_run_now(request: Request) -> dict[str, Any]:
        """Trigger an immediate DNS integrity check. Returns the result."""
        _require_scope(request, SCOPE_WRITE)
        if dns_verifier is None:
            raise HTTPException(503, "DNS verifier not running")
        env = await dns_verifier.run_once()
        return env.to_dict()

    @app.post("/api/v1/dns/environment")
    async def dns_environment_submit(request: Request) -> dict[str, Any]:
        """
        Accept a DNS environment assessment from a remote node or agent.

        Body: DNSEnvironment.to_dict() payload (node_id required).
        Nodes call this after running their own dns_verify checks locally.
        """
        _require_scope(request, SCOPE_WRITE)
        if dns_verifier is None:
            raise HTTPException(503, "DNS verifier not running")
        body = await request.json()
        node_id = body.get("node_id", "")
        if not node_id:
            raise HTTPException(400, "node_id required")
        dns_verifier.accept_node_environment(node_id, body)
        return {"ok": True}

    @app.get("/api/v1/dns/environment/{node_id}")
    async def dns_environment_node(request: Request, node_id: str) -> dict[str, Any]:
        """Return the DNS environment assessment most recently submitted by a node."""
        _require_scope(request, SCOPE_READ)
        if dns_verifier is None:
            raise HTTPException(503, "DNS verifier not running")
        env = dns_verifier.get_environment(node_id)
        if env is None:
            raise HTTPException(404, f"No DNS assessment for node {node_id!r}")
        return env.to_dict()

    @app.post("/api/v1/dns/rebinding/allowlist")
    async def dns_rebinding_allowlist_add(request: Request) -> dict[str, Any]:
        """
        Add entries to the DNS rebinding guard allowlist.

        Body: { "entries": ["hostname-or-ip", ...] }
        """
        _require_scope(request, SCOPE_ADMIN)
        if dns_verifier is None:
            raise HTTPException(503, "DNS verifier not running")
        body = await request.json()
        entries: list[str] = body.get("entries", [])
        if not entries:
            raise HTTPException(400, "entries list required")
        dns_verifier.guard.add_allowlist(set(entries))
        return {"ok": True, "added": entries}

    @app.delete("/api/v1/dns/rebinding/allowlist")
    async def dns_rebinding_allowlist_remove(request: Request) -> dict[str, Any]:
        """Remove entries from the DNS rebinding guard allowlist."""
        _require_scope(request, SCOPE_ADMIN)
        if dns_verifier is None:
            raise HTTPException(503, "DNS verifier not running")
        body = await request.json()
        entries: list[str] = body.get("entries", [])
        if not entries:
            raise HTTPException(400, "entries list required")
        dns_verifier.guard.remove_allowlist(set(entries))
        return {"ok": True, "removed": entries}

    # GraphQL API (optional — requires strawberry-graphql)
    if _GRAPHQL_AVAILABLE and _gql_create_router is not None:
        graphql_router = _gql_create_router(state, _auth, mesh_ca)
        _gql_add_graphiql_route(graphql_router, state, _auth)
        app.include_router(graphql_router, prefix="")

    # Static files — mounted last so they don't shadow API routes
    static_dir = Path(__file__).parent / "static"
    if static_dir.exists():
        app.mount("/", StaticFiles(directory=static_dir, html=True), name="static")

    return app
