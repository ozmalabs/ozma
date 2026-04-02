# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Built-in Identity Provider — OIDC-compatible authentication.

Provides centralised auth for the controller, proxied services, and
LAN applications.  Social login (Google, Apple, GitHub), password auth,
and enterprise federation (AD/Entra/LDAP) all flow through here.

When enabled, the controller becomes an OIDC provider that any service
on the LAN can use for single sign-on.

Implementation choice is deliberately abstract — the commitment is
"controller is your IdP", not a specific product (Authentik, Keycloak, etc.).
"""

from __future__ import annotations

import hashlib
import html
import json
import logging
import secrets
import time
import urllib.parse
from dataclasses import dataclass, field
from typing import Any, TYPE_CHECKING

from fastapi import Request
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse

if TYPE_CHECKING:
    from users import UserManager, User
    from transport import IdentityKeyPair

log = logging.getLogger("ozma.idp")


# ── Configuration ────────────────────────────────────────────────────────

@dataclass
class SocialProvider:
    type: str            # "google" | "apple" | "github"
    client_id: str
    client_secret: str
    # Derived at runtime
    authorize_url: str = ""
    token_url: str = ""

    @classmethod
    def from_dict(cls, d: dict) -> SocialProvider:
        p = cls(
            type=d["type"],
            client_id=d.get("client_id", ""),
            client_secret=d.get("client_secret", ""),
        )
        # Set well-known OAuth2 URLs
        if p.type == "google":
            p.authorize_url = "https://accounts.google.com/o/oauth2/v2/auth"
            p.token_url = "https://oauth2.googleapis.com/token"
        elif p.type == "github":
            p.authorize_url = "https://github.com/login/oauth/authorize"
            p.token_url = "https://github.com/login/oauth/access_token"
        elif p.type == "apple":
            p.authorize_url = "https://appleid.apple.com/auth/authorize"
            p.token_url = "https://appleid.apple.com/auth/token"
        return p


@dataclass
class IdPConfig:
    enabled: bool = False
    issuer_url: str = ""                 # e.g. https://alice.c.ozma.dev
    social_providers: list[SocialProvider] = field(default_factory=list)
    federation: dict = field(default_factory=dict)
    # {"type": "entra"|"ldap"|"saml", "tenant_id": "...", ...}
    session_ttl_seconds: int = 86400     # 24 hours
    refresh_ttl_seconds: int = 604800    # 7 days
    allow_self_registration: bool = False

    @classmethod
    def from_dict(cls, d: dict) -> IdPConfig:
        providers = [SocialProvider.from_dict(p) for p in d.get("social_providers", [])]
        return cls(
            enabled=d.get("enabled", False),
            issuer_url=d.get("issuer_url", ""),
            social_providers=providers,
            federation=d.get("federation", {}),
            session_ttl_seconds=d.get("session_ttl_seconds", 86400),
            refresh_ttl_seconds=d.get("refresh_ttl_seconds", 604800),
            allow_self_registration=d.get("allow_self_registration", False),
        )


# ── Session management ───────────────────────────────────────────────────

@dataclass
class Session:
    id: str
    user_id: str
    created_at: float
    expires_at: float
    ip_address: str = ""

    @property
    def expired(self) -> bool:
        return time.time() > self.expires_at


# ── Identity Provider ────────────────────────────────────────────────────

class IdentityProvider:
    """Built-in OIDC-compatible identity provider.

    Provides:
      - Password authentication (local users)
      - Social login (Google, Apple, GitHub via OAuth2)
      - Session management (cookie-based)
      - OIDC discovery + JWKS endpoint (LAN services can use as IdP)
      - Enterprise federation (AD/Entra/LDAP — delegates to external IdP)
    """

    SESSION_COOKIE = "ozma_session"

    @staticmethod
    def _safe_redirect(url: str) -> str:
        """Sanitise a redirect URL — only allow relative paths to prevent open redirect."""
        if not url or url.startswith("//") or ":" in url.split("/")[0]:
            return "/"
        # Must start with /
        if not url.startswith("/"):
            return "/"
        return url

    def __init__(self, config: IdPConfig, user_manager: UserManager,
                 signing_key: IdentityKeyPair | None = None) -> None:
        self._config = config
        self._user_manager = user_manager
        self._signing_key = signing_key
        self._sessions: dict[str, Session] = {}   # session_id → Session
        # OAuth2 state tokens (CSRF protection for social login)
        self._oauth_states: dict[str, dict] = {}  # state → {provider, created_at}

    @property
    def enabled(self) -> bool:
        return self._config.enabled

    # ── Session management ───────────────────────────────────────────

    _MAX_SESSIONS = 10_000         # hard cap to prevent memory exhaustion
    _MAX_SESSIONS_PER_USER = 50    # per-user cap

    def create_session(self, user_id: str, ip_address: str = "") -> str:
        """Create a new session and return the session ID (for cookie)."""
        self._cleanup_expired()

        # Enforce global session cap
        if len(self._sessions) >= self._MAX_SESSIONS:
            # Evict oldest sessions
            oldest = sorted(self._sessions.values(), key=lambda s: s.created_at)
            for s in oldest[:len(self._sessions) - self._MAX_SESSIONS + 1]:
                del self._sessions[s.id]

        # Enforce per-user cap
        user_sessions = [s for s in self._sessions.values() if s.user_id == user_id]
        if len(user_sessions) >= self._MAX_SESSIONS_PER_USER:
            oldest = sorted(user_sessions, key=lambda s: s.created_at)
            for s in oldest[:len(user_sessions) - self._MAX_SESSIONS_PER_USER + 1]:
                del self._sessions[s.id]

        session_id = secrets.token_urlsafe(32)
        session = Session(
            id=session_id,
            user_id=user_id,
            created_at=time.time(),
            expires_at=time.time() + self._config.session_ttl_seconds,
            ip_address=ip_address,
        )
        self._sessions[session_id] = session
        log.info("Session created for user %s", user_id)
        return session_id

    def validate_session(self, session_id: str) -> str | None:
        """Validate a session cookie. Returns user_id or None."""
        session = self._sessions.get(session_id)
        if not session or session.expired:
            if session:
                del self._sessions[session_id]
            return None
        return session.user_id

    def validate_session_from_request(self, request: Request) -> str | None:
        """Extract and validate session from request cookie."""
        cookie = request.cookies.get(self.SESSION_COOKIE)
        if not cookie:
            return None
        return self.validate_session(cookie)

    def revoke_session(self, session_id: str) -> None:
        self._sessions.pop(session_id, None)

    def _cleanup_expired(self) -> None:
        now = time.time()
        expired = [sid for sid, s in self._sessions.items() if s.expires_at < now]
        for sid in expired:
            del self._sessions[sid]

    # ── OIDC discovery ───────────────────────────────────────────────

    def oidc_discovery(self) -> dict:
        """OpenID Connect discovery document."""
        issuer = self._config.issuer_url or "http://localhost:7380"
        return {
            "issuer": issuer,
            "authorization_endpoint": f"{issuer}/auth/authorize",
            "token_endpoint": f"{issuer}/auth/token",
            "userinfo_endpoint": f"{issuer}/auth/userinfo",
            "jwks_uri": f"{issuer}/auth/jwks",
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["EdDSA"],
            "scopes_supported": ["openid", "profile", "email"],
            "grant_types_supported": ["authorization_code"],
        }

    def jwks(self) -> dict:
        """JSON Web Key Set — exposes the controller's Ed25519 public key."""
        if not self._signing_key:
            return {"keys": []}
        import base64
        pub_b64 = base64.urlsafe_b64encode(self._signing_key.public_key).rstrip(b"=").decode()
        return {
            "keys": [{
                "kty": "OKP",
                "crv": "Ed25519",
                "use": "sig",
                "kid": hashlib.sha256(self._signing_key.public_key).hexdigest()[:16],
                "x": pub_b64,
            }],
        }

    # ── Login page ───────────────────────────────────────────────────

    def login_page(self, error: str = "", redirect_to: str = "/") -> HTMLResponse:
        """Render the login page."""
        # Security: sanitise redirect and HTML-escape all user-controlled values
        redirect_to = self._safe_redirect(redirect_to)
        safe_redirect = html.escape(redirect_to, quote=True)
        safe_error = html.escape(error) if error else ""

        social_buttons = ""
        for p in self._config.social_providers:
            label = html.escape(p.type.title())
            social_buttons += (
                f'<a href="/auth/login/{html.escape(p.type)}?redirect_to={safe_redirect}" '
                f'class="social-btn social-{html.escape(p.type)}">Sign in with {label}</a>\n'
            )

        error_html = f'<div class="error">{safe_error}</div>' if safe_error else ""

        page = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Ozma — Sign In</title>
<style>
*, *::before, *::after {{ box-sizing: border-box; margin: 0; padding: 0; }}
body {{
    background: #050505;
    color: #f0fdf6;
    font-family: 'Inter', system-ui, sans-serif;
    display: flex; align-items: center; justify-content: center;
    min-height: 100vh;
}}
.card {{
    background: rgba(20,20,20,0.92);
    border: 1px solid rgba(74,224,164,0.11);
    border-radius: 20px;
    padding: 40px;
    width: 380px;
    max-width: 90vw;
}}
.logo {{
    font-size: 20px; font-weight: 800; letter-spacing: -0.03em;
    margin-bottom: 32px; text-align: center;
}}
.logo span {{ color: #4ae0a4; }}
h2 {{ font-size: 16px; font-weight: 600; margin-bottom: 20px; text-align: center; }}
.error {{ color: #f26464; font-size: 13px; margin-bottom: 16px; text-align: center; }}
form {{ display: flex; flex-direction: column; gap: 12px; }}
input {{
    padding: 12px 16px; border-radius: 10px;
    border: 1px solid rgba(74,224,164,0.2);
    background: rgba(255,255,255,0.05);
    color: #f0fdf6; font-size: 14px; font-family: inherit;
    outline: none;
}}
input:focus {{ border-color: #4ae0a4; box-shadow: 0 0 0 3px rgba(74,224,164,0.1); }}
button {{
    padding: 12px; border-radius: 10px; border: none;
    background: #4ae0a4; color: #050505;
    font-size: 14px; font-weight: 700; font-family: inherit;
    cursor: pointer;
}}
button:hover {{ background: #6ef5bc; }}
.divider {{
    display: flex; align-items: center; gap: 12px;
    margin: 20px 0; color: rgba(255,255,255,0.3); font-size: 12px;
}}
.divider::before, .divider::after {{
    content: ""; flex: 1; border-top: 1px solid rgba(255,255,255,0.1);
}}
.social-btn {{
    display: block; padding: 10px; border-radius: 10px;
    border: 1px solid rgba(255,255,255,0.1);
    color: #f0fdf6; font-size: 13px; font-weight: 600;
    text-decoration: none; text-align: center;
    margin-bottom: 8px;
}}
.social-btn:hover {{ border-color: rgba(74,224,164,0.3); background: rgba(255,255,255,0.03); }}
</style>
</head>
<body>
<div class="card">
    <div class="logo">ozma<span>labs</span></div>
    <h2>Sign in to your controller</h2>
    {error_html}
    <form method="POST" action="/auth/login">
        <input type="hidden" name="redirect_to" value="{safe_redirect}">
        <input type="text" name="username" placeholder="Username" required autocomplete="username">
        <input type="password" name="password" placeholder="Password" required autocomplete="current-password">
        <button type="submit">Sign in</button>
    </form>
    {('<div class="divider">or</div>' + social_buttons) if social_buttons else ""}
</div>
</body>
</html>"""
        return HTMLResponse(content=page)

    # ── Password login ───────────────────────────────────────────────

    async def handle_login(self, request: Request) -> RedirectResponse | HTMLResponse:
        """Handle POST /auth/login — password authentication."""
        form = await request.form()
        username = str(form.get("username", ""))
        password = str(form.get("password", ""))
        redirect_to = self._safe_redirect(str(form.get("redirect_to", "/")))

        user = self._user_manager.authenticate(username, password)
        if not user:
            return self.login_page(error="Invalid username or password",
                                   redirect_to=redirect_to)

        client_ip = request.client.host if request.client else "127.0.0.1"
        session_id = self.create_session(user.id, ip_address=client_ip)
        response = RedirectResponse(url=redirect_to, status_code=303)
        use_secure = self._config.issuer_url.startswith("https")
        response.set_cookie(
            self.SESSION_COOKIE, session_id,
            httponly=True, samesite="lax", secure=use_secure,
            max_age=self._config.session_ttl_seconds,
        )
        return response

    # ── Social login ─────────────────────────────────────────────────

    def social_redirect(self, provider_type: str, redirect_to: str = "/") -> RedirectResponse:
        """Redirect to social provider's OAuth2 authorize endpoint."""
        redirect_to = self._safe_redirect(redirect_to)
        provider = None
        for p in self._config.social_providers:
            if p.type == provider_type:
                provider = p
                break
        if not provider:
            return RedirectResponse(url="/auth/login?error=unknown_provider")

        state = secrets.token_urlsafe(32)
        self._oauth_states[state] = {
            "provider": provider_type,
            "redirect_to": redirect_to,
            "created_at": time.time(),
        }

        callback_url = f"{self._config.issuer_url}/auth/callback/{provider_type}"
        params = {
            "client_id": provider.client_id,
            "redirect_uri": callback_url,
            "response_type": "code",
            "state": state,
            "scope": "openid email profile",
        }
        query = urllib.parse.urlencode(params)
        return RedirectResponse(url=f"{provider.authorize_url}?{query}")

    async def social_callback(self, provider_type: str,
                               request: Request) -> RedirectResponse | HTMLResponse:
        """Handle OAuth2 callback from social provider."""
        code = request.query_params.get("code", "")
        state = request.query_params.get("state", "")

        state_data = self._oauth_states.pop(state, None)
        if not state_data or state_data["provider"] != provider_type:
            return self.login_page(error="Invalid OAuth state")

        # Clean up old states
        now = time.time()
        self._oauth_states = {
            k: v for k, v in self._oauth_states.items()
            if now - v["created_at"] < 600
        }

        redirect_to = state_data.get("redirect_to", "/")

        # TODO: Exchange code for token, get user info from provider
        # This requires an HTTP client call to the provider's token endpoint
        # For now, log and redirect with error
        log.info("Social callback from %s with code (exchange not yet implemented)", provider_type)
        return self.login_page(
            error=f"{provider_type.title()} login not yet fully implemented",
            redirect_to=redirect_to,
        )

    # ── Logout ───────────────────────────────────────────────────────

    def handle_logout(self, request: Request) -> RedirectResponse:
        """Handle POST /auth/logout."""
        cookie = request.cookies.get(self.SESSION_COOKIE)
        if cookie:
            self.revoke_session(cookie)
        response = RedirectResponse(url="/auth/login", status_code=303)
        response.delete_cookie(self.SESSION_COOKIE)
        return response

    # ── OIDC token endpoint ──────────────────────────────────────────

    async def token_endpoint(self, request: Request) -> JSONResponse:
        """OIDC token endpoint — authorization_code grant."""
        # TODO: implement authorization code flow for LAN OIDC clients
        return JSONResponse(
            status_code=501,
            content={"error": "not_implemented",
                     "description": "Authorization code flow not yet implemented"},
        )

    async def userinfo_endpoint(self, request: Request) -> JSONResponse:
        """OIDC userinfo endpoint — returns claims for the authenticated user."""
        user_id = self.validate_session_from_request(request)
        if not user_id:
            return JSONResponse(status_code=401, content={"error": "unauthenticated"})
        user = self._user_manager.get_user(user_id)
        if not user:
            return JSONResponse(status_code=404, content={"error": "user_not_found"})
        return JSONResponse(content={
            "sub": user.id,
            "preferred_username": user.username,
            "name": user.display_name,
            "email": user.email,
        })

    # ── Stats ────────────────────────────────────────────────────────

    def active_session_count(self) -> int:
        self._cleanup_expired()
        return len(self._sessions)
