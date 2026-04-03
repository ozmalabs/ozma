# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
REST API and WebSocket server on port 7380.

REST endpoints:
  GET  /api/v1/nodes                 — list all known nodes
  GET  /api/v1/nodes/{id}            — get a single node
  POST /api/v1/nodes/{id}/activate   — make a node active
  GET  /api/v1/status                — system snapshot

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

from state import AppState, NodeInfo
from permissions import (
    check_node_permission, check_destructive_warnings, warnings_to_dict,
    get_user_seats, get_user_permission_level, PERMISSION_LEVELS,
)
from scenarios import ScenarioManager
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
from job_queue import JobQueue, JobSpec, JobType, JobState, TargetScope

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


def build_app(state: AppState, scenarios: ScenarioManager, streams: StreamManager | None = None, audio: AudioRouter | None = None, controls: ControlManager | None = None, rgb_out: RGBOutputManager | None = None, motion: MotionManager | None = None, bt: BluetoothManager | None = None, kdeconnect: KDEConnectBridge | None = None, wifi_audio: WiFiAudioManager | None = None, captures: DisplayCaptureManager | None = None, paste_typer: PasteTyper | None = None, kbd_mgr: KeyboardManager | None = None, macro_mgr: MacroManager | None = None, sched: Scheduler | None = None, notifier: NotificationManager | None = None, recorder: SessionRecorder | None = None, net_health: NetworkHealthMonitor | None = None, ocr_triggers: OCRTriggerManager | None = None, auto_engine: AutomationEngine | None = None, metrics_collector: MetricsCollector | None = None, screen_mgr: ScreenManager | None = None, codec_mgr: CodecManager | None = None, camera_mgr: CameraManager | None = None, obs_studio: OBSStudioManager | None = None, stream_router: StreamRouter | None = None, guac_mgr: GuacamoleManager | None = None, provision_mgr: ProvisioningManager | None = None, connect: OzmaConnect | None = None, mesh_ca: MeshCA | None = None, sess_mgr: SessionManager | None = None, room_correction: Any = None, testbench: Any = None, agent_engine: Any = None, test_runner: Any = None, auth_config: AuthConfig | None = None, user_manager: UserManager | None = None, service_proxy: ServiceProxyManager | None = None, idp: IdentityProvider | None = None, sharing: SharingManager | None = None, ext_publish: ExternalPublishManager | None = None, node_reconciler=None, update_mgr=None, transcription_mgr=None, discovery=None, doorbell_mgr=None, alert_mgr=None, vaultwarden: VaultwardenManager | None = None, email_security: EmailSecurityMonitor | None = None, cloud_backup: CloudBackupManager | None = None, iot: IoTNetworkManager | None = None, wg: WGPeeringManager | None = None, itsm: ITSMManager | None = None, license_mgr: LicenseManager | None = None, mdm: MDMBridgeManager | None = None, job_queue: JobQueue | None = None) -> FastAPI:
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

    # --- Identity Provider routes ---

    @app.get("/.well-known/openid-configuration")
    async def oidc_discovery() -> dict:
        if not idp or not idp.enabled:
            raise HTTPException(404, "IdP not enabled")
        return idp.oidc_discovery()

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
        return {
            "nodes": [n.to_dict() for n in state.nodes.values()],
            "active_node_id": state.active_node_id,
        }

    @app.get("/api/v1/nodes/{node_id}")
    async def get_node(node_id: str) -> dict[str, Any]:
        node = state.nodes.get(node_id)
        if node is None:
            raise HTTPException(status_code=404, detail="Node not found")
        return node.to_dict()

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
        )
        if not profile:
            return {"ok": False, "error": "Sweep failed — check source/sink names and PipeWire state"}
        return {"ok": True, "profile": profile.to_dict()}

    @app.post("/api/v1/audio/room-correction/measure")
    async def process_room_measurement(body: dict) -> dict[str, Any]:
        """
        Process a room measurement from the phone sweep UI.

        Body: {
            frequency_response: [[freq, db], ...],  // from browser FFT
            phone_model: "iphone_15" | "pixel_8" | "generic" | ...,
            target_curve: "harman" | "flat" | "bbc",
            room_name: "Living Room",
            node_id: "vm1._ozma._udp.local."
        }
        """
        if not room_correction:
            raise HTTPException(status_code=503, detail="Room correction not available")
        freq_resp = [(f, db) for f, db in body.get("frequency_response", [])]
        if not freq_resp:
            raise HTTPException(status_code=400, detail="frequency_response required")
        profile = room_correction.process_measurement(
            frequency_response=freq_resp,
            phone_model=body.get("phone_model", "generic"),
            target_curve=body.get("target_curve", "harman"),
            room_name=body.get("room_name", ""),
            node_id=body.get("node_id", ""),
        )
        return {"ok": True, "profile": profile.to_dict()}

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
        """Execute an automation script."""
        if not auto_engine:
            raise HTTPException(status_code=503, detail="Automation engine not available")
        script = body.get("script", "")
        if not script:
            raise HTTPException(status_code=400, detail="No script provided")
        result = await auto_engine.run_script(
            script,
            source_id=body.get("source_id", ""),
            variables=body.get("variables"),
        )
        return result

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
    async def get_camera(camera_id: str) -> dict[str, Any]:
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

    # --- OBS / Broadcast studio endpoints ---

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
            return {"authenticated": False, "tier": "free"}
        return connect.status()

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

    # --- Replay buffer endpoints ---

    @app.get("/api/v1/replay/status")
    async def replay_status() -> dict[str, Any]:
        return {"enabled": True, "sources": []}

    @app.post("/api/v1/replay/save")
    async def replay_save(body: dict = {}) -> dict[str, Any]:
        return {"ok": False, "message": "No active capture sources for replay"}

    # --- Notification endpoints ---

    @app.get("/api/v1/notifications")
    async def notification_list() -> dict[str, Any]:
        if not notifier:
            return {"channels": [], "recent": []}
        return {"channels": notifier.list_channels() if hasattr(notifier, 'list_channels') else [],
                "recent": []}

    @app.post("/api/v1/notifications/test")
    async def notification_test(body: dict = {}) -> dict[str, Any]:
        if not notifier:
            raise HTTPException(status_code=503, detail="Notifications not available")
        return {"ok": True, "message": "Test notification sent"}

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
        return vaultwarden.get_status()

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

    # Static files — mounted last so they don't shadow API routes
    static_dir = Path(__file__).parent / "static"
    if static_dir.exists():
        app.mount("/", StaticFiles(directory=static_dir, html=True), name="static")

    return app
