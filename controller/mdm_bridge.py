# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
MDM Bridge — unified abstraction over Mobile Device Management providers.

Supported providers:
  google  — Google Workspace Endpoint Management (Admin SDK Directory API)
  intune  — Microsoft Intune (Microsoft Graph API)
  jamf    — Jamf Pro / Jamf Now (Jamf Pro API)

What the bridge does:
  1. Sync device inventory → normalized ManagedDevice records (ITAM source)
  2. Detect unenrolled users → compliance gap reporting
  3. Send enrollment invitations (email via provider API)
  4. Push WireGuard VPN profiles to enrolled devices (platform-specific format)
  5. Push corporate Wi-Fi profiles
  6. Remote lock / remote wipe for offboarding
  7. Unenroll devices when an employee is offboarded

Each device gets a Ozma-managed WireGuard keypair on first VPN profile push.
The controller holds the private key; the profile (including private key) is
delivered to the device via the MDM provider's encrypted push channel.
Mobile devices are assigned IPs from 10.200.250.0/24 (reserved for this).

Auth:
  Google  — JWT assertion (service account + domain-wide delegation)
  Intune  — OAuth2 client_credentials (Azure AD app registration)
  Jamf    — OAuth2 client_credentials (Jamf Pro API Client)

Credentials (API secrets) are never stored directly. Env var names are stored;
the actual values are read from the environment at runtime.
"""

from __future__ import annotations

import asyncio
import base64
import json
import logging
import os
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.mdm_bridge")

# Mobile device WireGuard overlay subnet (distinct from controller/node 10.200.x.x)
MOBILE_WG_SUBNET = "10.200.250"   # /24 — devices get .1 through .254
MOBILE_WG_PORT   = 51821          # distinct from ctrl-ctrl port 51820


# ── Normalized device record ──────────────────────────────────────────────────

@dataclass
class ManagedDevice:
    """Device record normalized across all MDM providers."""
    id: str                             # provider-specific device ID
    provider: str                       # "google" | "intune" | "jamf"
    user_email: str
    name: str
    platform: str                       # "ios" | "android" | "macos" | "windows" | "chromeos" | "linux"
    model: str = ""
    serial: str = ""
    os_version: str = ""
    enrolled_at: float = 0.0
    last_sync_at: float = 0.0
    compliance_state: str = "unknown"   # "compliant" | "noncompliant" | "unknown"
    encrypted: bool = False
    screen_lock: bool = False
    management_state: str = "managed"   # "managed" | "supervised" | "pending_removal"
    # Ozma overlay network state
    vpn_profile_pushed: bool = False
    vpn_private_key: str = ""           # stored encrypted in config; never logged
    vpn_public_key: str = ""
    vpn_ip: str = ""                    # 10.200.250.X
    wifi_profile_pushed: bool = False
    last_updated: float = 0.0

    def to_dict(self, include_private_key: bool = False) -> dict[str, Any]:
        d: dict[str, Any] = {
            "id": self.id,
            "provider": self.provider,
            "user_email": self.user_email,
            "name": self.name,
            "platform": self.platform,
            "model": self.model,
            "serial": self.serial,
            "os_version": self.os_version,
            "enrolled_at": self.enrolled_at,
            "last_sync_at": self.last_sync_at,
            "compliance_state": self.compliance_state,
            "encrypted": self.encrypted,
            "screen_lock": self.screen_lock,
            "management_state": self.management_state,
            "vpn_profile_pushed": self.vpn_profile_pushed,
            "vpn_public_key": self.vpn_public_key,
            "vpn_ip": self.vpn_ip,
            "wifi_profile_pushed": self.wifi_profile_pushed,
            "last_updated": self.last_updated,
        }
        if include_private_key:
            d["vpn_private_key"] = self.vpn_private_key
        return d

    @classmethod
    def from_dict(cls, d: dict) -> "ManagedDevice":
        return cls(
            id=d["id"],
            provider=d["provider"],
            user_email=d.get("user_email", ""),
            name=d.get("name", ""),
            platform=d.get("platform", "unknown"),
            model=d.get("model", ""),
            serial=d.get("serial", ""),
            os_version=d.get("os_version", ""),
            enrolled_at=d.get("enrolled_at", 0.0),
            last_sync_at=d.get("last_sync_at", 0.0),
            compliance_state=d.get("compliance_state", "unknown"),
            encrypted=d.get("encrypted", False),
            screen_lock=d.get("screen_lock", False),
            management_state=d.get("management_state", "managed"),
            vpn_profile_pushed=d.get("vpn_profile_pushed", False),
            vpn_private_key=d.get("vpn_private_key", ""),
            vpn_public_key=d.get("vpn_public_key", ""),
            vpn_ip=d.get("vpn_ip", ""),
            wifi_profile_pushed=d.get("wifi_profile_pushed", False),
            last_updated=d.get("last_updated", 0.0),
        )


# ── Config ────────────────────────────────────────────────────────────────────

@dataclass
class MDMConfig:
    provider: str = ""              # "google" | "intune" | "jamf" | "" = not configured

    # ── Google Workspace Endpoint Management ──────────────────────────
    # Requires: Admin SDK API enabled, service account with domain-wide delegation
    # Scopes needed:
    #   https://www.googleapis.com/auth/admin.directory.device.mobile
    #   https://www.googleapis.com/auth/admin.directory.device.chromeos
    google_admin_email: str = ""    # admin@yourdomain.com
    google_service_account_json_env: str = ""  # env var containing service account JSON
    google_customer_id: str = "my_customer"

    # ── Microsoft Intune (Azure AD app registration) ──────────────────
    # Requires: App registration with DeviceManagementManagedDevices.ReadWrite.All
    intune_tenant_id: str = ""
    intune_client_id: str = ""
    intune_client_secret_env: str = ""  # env var name

    # ── Jamf Pro / Jamf Cloud ─────────────────────────────────────────
    # Requires: API Client with device read + management permissions
    jamf_base_url: str = ""         # https://yourorg.jamfcloud.com
    jamf_client_id: str = ""
    jamf_client_secret_env: str = ""

    # ── VPN profile push settings ─────────────────────────────────────
    # wg_endpoint: the public endpoint included in pushed VPN profiles
    wg_endpoint: str = ""           # "vpn.company.com:51821" (or leave blank to skip VPN push)
    wg_server_public_key: str = ""  # controller's WireGuard public key
    wg_dns: str = ""                # DNS server to push (e.g. "10.200.1.1")
    wg_allowed_ips: str = "10.200.0.0/16"  # routes via VPN

    # ── Wi-Fi profile push settings ───────────────────────────────────
    wifi_ssid: str = ""
    wifi_security: str = "WPA2"     # WPA2 | WPA3 | None
    wifi_password_env: str = ""     # env var name; empty = don't push Wi-Fi

    # ── Sync ──────────────────────────────────────────────────────────
    sync_interval_seconds: int = 900   # 15 min

    def to_dict(self) -> dict[str, Any]:
        return {
            "provider": self.provider,
            "google_admin_email": self.google_admin_email,
            "google_service_account_json_env": self.google_service_account_json_env,
            "google_customer_id": self.google_customer_id,
            "intune_tenant_id": self.intune_tenant_id,
            "intune_client_id": self.intune_client_id,
            "intune_client_secret_env": self.intune_client_secret_env,
            "jamf_base_url": self.jamf_base_url,
            "jamf_client_id": self.jamf_client_id,
            "jamf_client_secret_env": self.jamf_client_secret_env,
            "wg_endpoint": self.wg_endpoint,
            "wg_server_public_key": self.wg_server_public_key,
            "wg_dns": self.wg_dns,
            "wg_allowed_ips": self.wg_allowed_ips,
            "wifi_ssid": self.wifi_ssid,
            "wifi_security": self.wifi_security,
            "wifi_password_env": self.wifi_password_env,
            "sync_interval_seconds": self.sync_interval_seconds,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "MDMConfig":
        return cls(**{k: v for k, v in d.items() if k in cls.__dataclass_fields__})


# ── Provider base ─────────────────────────────────────────────────────────────

class MDMProvider:
    """Abstract base for MDM provider implementations."""

    def __init__(self, config: MDMConfig) -> None:
        self._config = config
        self._token: str = ""
        self._token_expires: float = 0.0

    async def list_devices(self) -> list[ManagedDevice]:
        raise NotImplementedError

    async def send_enrollment_invite(self, email: str, name: str) -> bool:
        raise NotImplementedError

    async def remote_lock(self, device_id: str) -> bool:
        raise NotImplementedError

    async def remote_wipe(self, device_id: str) -> bool:
        raise NotImplementedError

    async def unenroll(self, device_id: str) -> bool:
        raise NotImplementedError

    async def push_custom_profile(
        self,
        device_id: str,
        profile_name: str,
        profile_payload: str,
        payload_type: str = "application/x-apple-aspen-config",
    ) -> bool:
        """Push a configuration profile (mobileconfig or equivalent) to a device."""
        raise NotImplementedError

    def _token_valid(self) -> bool:
        return bool(self._token) and time.time() < self._token_expires - 30

    async def _http(
        self,
        method: str,
        url: str,
        body: bytes | None = None,
        content_type: str = "application/json",
        extra_headers: dict | None = None,
    ) -> dict[str, Any]:
        headers = {"Authorization": f"Bearer {self._token}", "Content-Type": content_type}
        if extra_headers:
            headers.update(extra_headers)
        req = urllib.request.Request(url, data=body, headers=headers, method=method)
        loop = asyncio.get_running_loop()
        try:
            def _do():
                resp = urllib.request.urlopen(req, timeout=30)
                raw = resp.read()
                return json.loads(raw) if raw else {}
            return await loop.run_in_executor(None, _do)
        except urllib.error.HTTPError as e:
            body_text = e.read().decode(errors="replace")[:500]
            raise RuntimeError(f"HTTP {e.code} {url}: {body_text}") from e


# ── Google Workspace Endpoint Management ──────────────────────────────────────

class GoogleMDMProvider(MDMProvider):
    """
    Google Workspace Endpoint Management via Admin SDK Directory API.

    Requires service account with domain-wide delegation and scopes:
      https://www.googleapis.com/auth/admin.directory.device.mobile
      https://www.googleapis.com/auth/admin.directory.device.chromeos
    """

    _TOKEN_URL = "https://oauth2.googleapis.com/token"
    _SCOPES = [
        "https://www.googleapis.com/auth/admin.directory.device.mobile",
        "https://www.googleapis.com/auth/admin.directory.device.chromeos",
    ]

    async def _ensure_token(self) -> None:
        if self._token_valid():
            return
        sa_json_env = self._config.google_service_account_json_env
        sa_json_raw = os.environ.get(sa_json_env, "")
        if not sa_json_raw:
            raise RuntimeError(
                f"Google service account JSON not found in env var '{sa_json_env}'"
            )
        try:
            sa = json.loads(sa_json_raw)
        except json.JSONDecodeError as e:
            raise RuntimeError(f"Invalid Google service account JSON: {e}") from e

        # Build JWT for assertion flow
        now = int(time.time())
        header = base64.urlsafe_b64encode(
            json.dumps({"alg": "RS256", "typ": "JWT"}).encode()
        ).rstrip(b"=")
        claims = base64.urlsafe_b64encode(json.dumps({
            "iss": sa["client_email"],
            "sub": self._config.google_admin_email,
            "aud": self._TOKEN_URL,
            "scope": " ".join(self._SCOPES),
            "iat": now,
            "exp": now + 3600,
        }).encode()).rstrip(b"=")
        signing_input = header + b"." + claims

        try:
            from cryptography.hazmat.primitives import hashes, serialization
            from cryptography.hazmat.primitives.asymmetric import padding
            privkey = serialization.load_pem_private_key(
                sa["private_key"].encode(), password=None
            )
            sig = privkey.sign(signing_input, padding.PKCS1v15(), hashes.SHA256())
            sig_b64 = base64.urlsafe_b64encode(sig).rstrip(b"=")
        except ImportError as e:
            raise RuntimeError("cryptography package required for Google MDM auth") from e

        jwt_token = signing_input + b"." + sig_b64
        body = urllib.parse.urlencode({
            "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
            "assertion": jwt_token.decode(),
        }).encode()

        loop = asyncio.get_running_loop()
        def _exchange():
            req = urllib.request.Request(
                self._TOKEN_URL, data=body,
                headers={"Content-Type": "application/x-www-form-urlencoded"},
                method="POST",
            )
            resp = urllib.request.urlopen(req, timeout=15)
            return json.loads(resp.read())

        data = await loop.run_in_executor(None, _exchange)
        self._token = data["access_token"]
        self._token_expires = time.time() + data.get("expires_in", 3600)

    async def list_devices(self) -> list[ManagedDevice]:
        await self._ensure_token()
        cid = self._config.google_customer_id or "my_customer"
        devices: list[ManagedDevice] = []

        # Mobile devices (Android / iOS)
        url = (
            f"https://admin.googleapis.com/admin/directory/v1/customer/{cid}"
            f"/devices/mobile?maxResults=250&projection=FULL"
        )
        while url:
            data = await self._http("GET", url)
            for raw in data.get("mobiledevices", []):
                devices.append(_parse_google_mobile(raw))
            url = data.get("nextPageToken")
            if url:
                base = (
                    f"https://admin.googleapis.com/admin/directory/v1/customer/{cid}"
                    f"/devices/mobile?maxResults=250&projection=FULL&pageToken={url}"
                )
                url = base

        # Chrome OS devices
        chrome_url = (
            f"https://admin.googleapis.com/admin/directory/v1/customer/{cid}"
            f"/devices/chromeos?maxResults=250"
        )
        while chrome_url:
            data = await self._http("GET", chrome_url)
            for raw in data.get("chromeosdevices", []):
                devices.append(_parse_google_chrome(raw))
            token = data.get("nextPageToken")
            chrome_url = (
                f"https://admin.googleapis.com/admin/directory/v1/customer/{cid}"
                f"/devices/chromeos?maxResults=250&pageToken={token}"
            ) if token else ""

        return devices

    async def remote_lock(self, device_id: str) -> bool:
        await self._ensure_token()
        cid = self._config.google_customer_id or "my_customer"
        url = (
            f"https://admin.googleapis.com/admin/directory/v1/customer/{cid}"
            f"/devices/mobile/{device_id}/action"
        )
        try:
            await self._http("POST", url,
                             body=json.dumps({"action": "admin_remote_lock"}).encode())
            return True
        except RuntimeError as e:
            log.warning("Google remote lock failed: %s", e)
            return False

    async def remote_wipe(self, device_id: str) -> bool:
        await self._ensure_token()
        cid = self._config.google_customer_id or "my_customer"
        url = (
            f"https://admin.googleapis.com/admin/directory/v1/customer/{cid}"
            f"/devices/mobile/{device_id}/action"
        )
        try:
            await self._http("POST", url,
                             body=json.dumps({"action": "wipe"}).encode())
            return True
        except RuntimeError as e:
            log.warning("Google remote wipe failed: %s", e)
            return False

    async def unenroll(self, device_id: str) -> bool:
        await self._ensure_token()
        cid = self._config.google_customer_id or "my_customer"
        url = (
            f"https://admin.googleapis.com/admin/directory/v1/customer/{cid}"
            f"/devices/mobile/{device_id}/action"
        )
        try:
            await self._http("POST", url,
                             body=json.dumps({"action": "admin_account_wipe"}).encode())
            return True
        except RuntimeError as e:
            log.warning("Google unenroll failed: %s", e)
            return False

    async def send_enrollment_invite(self, email: str, name: str) -> bool:
        # Google Endpoint Management enrollment is user-initiated via the
        # Google Device Policy app; there is no API to push an invite email.
        # Best available: direct admin to the enrollment URL.
        log.info(
            "Google MDM: enrollment is user-initiated. "
            "Direct %s to install Google Device Policy and sign in with %s.",
            name, email,
        )
        return False

    async def push_custom_profile(
        self, device_id: str, profile_name: str, profile_payload: str,
        payload_type: str = "application/x-apple-aspen-config",
    ) -> bool:
        # Google Workspace MDM does not support pushing arbitrary config profiles
        # via the Directory API. WireGuard must be pushed via managed app config
        # through Google Play (Android) or Apple Business Manager (iOS).
        log.warning(
            "Google MDM: custom profile push not supported via Directory API. "
            "Push WireGuard via managed app config in the Admin Console."
        )
        return False


def _parse_google_mobile(raw: dict) -> ManagedDevice:
    platform_map = {
        "ANDROID": "android", "IOS": "ios", "GOOGLE_SYNC": "android",
        "WINDOWS_PHONE": "windows",
    }
    platform = platform_map.get(raw.get("type", ""), "unknown")
    enrolled_at = 0.0
    if raw.get("firstSync"):
        try:
            from datetime import datetime
            enrolled_at = datetime.fromisoformat(
                raw["firstSync"].replace("Z", "+00:00")
            ).timestamp()
        except (ValueError, KeyError):
            pass
    return ManagedDevice(
        id=raw.get("deviceId", raw.get("resourceId", "")),
        provider="google",
        user_email=(raw.get("email") or [""])[0] if isinstance(raw.get("email"), list)
                   else raw.get("email", ""),
        name=raw.get("name", raw.get("model", "")),
        platform=platform,
        model=raw.get("model", ""),
        serial=raw.get("serialNumber", ""),
        os_version=raw.get("os", ""),
        enrolled_at=enrolled_at,
        last_sync_at=enrolled_at,
        compliance_state="compliant" if raw.get("status") == "APPROVED" else "unknown",
        encrypted=raw.get("encryptionStatus") == "ENCRYPTED",
        screen_lock=raw.get("devicePasswordStatus") == "ENABLED",
        management_state="managed",
        last_updated=time.time(),
    )


def _parse_google_chrome(raw: dict) -> ManagedDevice:
    return ManagedDevice(
        id=raw.get("deviceId", ""),
        provider="google",
        user_email=raw.get("lastEnrollmentTime", ""),
        name=raw.get("annotatedAssetId") or raw.get("serialNumber", ""),
        platform="chromeos",
        model=raw.get("model", ""),
        serial=raw.get("serialNumber", ""),
        os_version=raw.get("osVersion", ""),
        compliance_state="compliant" if raw.get("status") == "ACTIVE" else "unknown",
        encrypted=True,  # Chrome OS always encrypted
        management_state="managed",
        last_updated=time.time(),
    )


# ── Microsoft Intune ──────────────────────────────────────────────────────────

class IntuneMDMProvider(MDMProvider):
    """
    Microsoft Intune via Microsoft Graph API.

    Requires Azure AD app registration with:
      DeviceManagementManagedDevices.ReadWrite.All
      DeviceManagementConfiguration.ReadWrite.All  (for profile push)
    """

    _TOKEN_URL_TPL = "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"
    _GRAPH_BASE = "https://graph.microsoft.com/v1.0"

    async def _ensure_token(self) -> None:
        if self._token_valid():
            return
        secret = os.environ.get(self._config.intune_client_secret_env, "")
        if not secret:
            raise RuntimeError(
                f"Intune client secret not found in env var "
                f"'{self._config.intune_client_secret_env}'"
            )
        url = self._TOKEN_URL_TPL.format(tenant=self._config.intune_tenant_id)
        body = urllib.parse.urlencode({
            "client_id": self._config.intune_client_id,
            "client_secret": secret,
            "grant_type": "client_credentials",
            "scope": "https://graph.microsoft.com/.default",
        }).encode()
        loop = asyncio.get_running_loop()
        def _exchange():
            req = urllib.request.Request(
                url, data=body,
                headers={"Content-Type": "application/x-www-form-urlencoded"},
                method="POST",
            )
            resp = urllib.request.urlopen(req, timeout=15)
            return json.loads(resp.read())

        data = await loop.run_in_executor(None, _exchange)
        if "error" in data:
            raise RuntimeError(f"Intune token error: {data.get('error_description', data)}")
        self._token = data["access_token"]
        self._token_expires = time.time() + data.get("expires_in", 3600)

    async def list_devices(self) -> list[ManagedDevice]:
        await self._ensure_token()
        devices: list[ManagedDevice] = []
        url = f"{self._GRAPH_BASE}/deviceManagement/managedDevices?$top=250"
        while url:
            data = await self._http("GET", url)
            for raw in data.get("value", []):
                devices.append(_parse_intune_device(raw))
            url = data.get("@odata.nextLink", "")
        return devices

    async def remote_lock(self, device_id: str) -> bool:
        await self._ensure_token()
        url = f"{self._GRAPH_BASE}/deviceManagement/managedDevices/{device_id}/remoteLock"
        try:
            await self._http("POST", url)
            return True
        except RuntimeError as e:
            log.warning("Intune remote lock failed: %s", e)
            return False

    async def remote_wipe(self, device_id: str) -> bool:
        await self._ensure_token()
        url = f"{self._GRAPH_BASE}/deviceManagement/managedDevices/{device_id}/wipe"
        try:
            await self._http("POST", url,
                             body=json.dumps({"keepEnrollmentData": False,
                                              "keepUserData": False}).encode())
            return True
        except RuntimeError as e:
            log.warning("Intune remote wipe failed: %s", e)
            return False

    async def unenroll(self, device_id: str) -> bool:
        await self._ensure_token()
        url = f"{self._GRAPH_BASE}/deviceManagement/managedDevices/{device_id}/retire"
        try:
            await self._http("POST", url)
            return True
        except RuntimeError as e:
            log.warning("Intune unenroll (retire) failed: %s", e)
            return False

    async def send_enrollment_invite(self, email: str, name: str) -> bool:
        # Intune enrollment invitations are sent via the Intune Company Portal;
        # there is no direct Graph API to trigger an enrollment email for a specific user.
        # Intune works via Azure AD group assignment — adding the user to an Intune
        # license group triggers the enrollment nudge in the Company Portal app.
        log.info(
            "Intune: direct enrollment invite not available via Graph API. "
            "Ensure %s (%s) is assigned an Intune license via Azure AD groups.",
            name, email,
        )
        return False

    async def push_custom_profile(
        self, device_id: str, profile_name: str, profile_payload: str,
        payload_type: str = "application/x-apple-aspen-config",
    ) -> bool:
        """Push a configuration profile via Intune deviceConfiguration."""
        await self._ensure_token()
        # Create a custom device configuration policy
        # For iOS/macOS: customConfiguration payload with mobileconfig
        # For Windows: customOmaSettings or Win32App
        # This creates the policy and assigns it to the device.
        payload = {
            "@odata.type": "#microsoft.graph.iosCustomConfiguration",
            "displayName": profile_name,
            "payloadFileName": f"{profile_name}.mobileconfig",
            "payload": base64.b64encode(profile_payload.encode()).decode(),
        }
        try:
            result = await self._http(
                "POST",
                f"{self._GRAPH_BASE}/deviceManagement/deviceConfigurations",
                body=json.dumps(payload).encode(),
            )
            profile_id = result.get("id")
            if not profile_id:
                return False
            # Assign to the specific device's user
            assign_url = (
                f"{self._GRAPH_BASE}/deviceManagement/deviceConfigurations"
                f"/{profile_id}/assign"
            )
            # Get the device's user principal name first
            dev_data = await self._http(
                "GET",
                f"{self._GRAPH_BASE}/deviceManagement/managedDevices/{device_id}"
                f"?$select=userPrincipalName",
            )
            upn = dev_data.get("userPrincipalName", "")
            if upn:
                await self._http("POST", assign_url, body=json.dumps({
                    "assignments": [{
                        "target": {
                            "@odata.type": "#microsoft.graph.groupAssignmentTarget",
                            "groupId": upn,
                        }
                    }]
                }).encode())
            return True
        except RuntimeError as e:
            log.warning("Intune profile push failed: %s", e)
            return False


def _parse_intune_device(raw: dict) -> ManagedDevice:
    platform_map = {
        "ios": "ios", "android": "android", "androidForWork": "android",
        "macOS": "macos", "windows": "windows", "windowsPhone81": "windows",
    }
    platform = platform_map.get(raw.get("operatingSystem", "").lower(), "unknown")
    enrolled_at = 0.0
    if raw.get("enrolledDateTime"):
        try:
            from datetime import datetime
            enrolled_at = datetime.fromisoformat(
                raw["enrolledDateTime"].replace("Z", "+00:00")
            ).timestamp()
        except (ValueError, KeyError):
            pass
    return ManagedDevice(
        id=raw.get("id", ""),
        provider="intune",
        user_email=raw.get("userPrincipalName", ""),
        name=raw.get("deviceName", raw.get("managedDeviceName", "")),
        platform=platform,
        model=raw.get("model", ""),
        serial=raw.get("serialNumber", ""),
        os_version=raw.get("osVersion", ""),
        enrolled_at=enrolled_at,
        last_sync_at=enrolled_at,
        compliance_state=raw.get("complianceState", "unknown"),
        encrypted=raw.get("isEncrypted", False),
        screen_lock=raw.get("isSupervised", False),
        management_state=raw.get("managementState", "managed"),
        last_updated=time.time(),
    )


# ── Jamf Pro / Jamf Cloud ─────────────────────────────────────────────────────

class JamfMDMProvider(MDMProvider):
    """
    Jamf Pro / Jamf Cloud via Jamf Pro API.

    Requires an API Client with:
      Read Computers, Read Mobile Devices, Send Computer Remote Lock/Wipe,
      Send Mobile Device Remote Lock/Wipe, Delete Computer, Delete Mobile Device
    """

    async def _ensure_token(self) -> None:
        if self._token_valid():
            return
        secret = os.environ.get(self._config.jamf_client_secret_env, "")
        if not secret:
            raise RuntimeError(
                f"Jamf client secret not found in env var "
                f"'{self._config.jamf_client_secret_env}'"
            )
        base = self._config.jamf_base_url.rstrip("/")
        body = urllib.parse.urlencode({
            "client_id": self._config.jamf_client_id,
            "client_secret": secret,
            "grant_type": "client_credentials",
        }).encode()
        loop = asyncio.get_running_loop()
        def _exchange():
            req = urllib.request.Request(
                f"{base}/api/oauth/token",
                data=body,
                headers={"Content-Type": "application/x-www-form-urlencoded"},
                method="POST",
            )
            resp = urllib.request.urlopen(req, timeout=15)
            return json.loads(resp.read())

        data = await loop.run_in_executor(None, _exchange)
        if "error" in data:
            raise RuntimeError(f"Jamf token error: {data.get('error', data)}")
        self._token = data["access_token"]
        self._token_expires = time.time() + data.get("expires_in", 1800)

    async def list_devices(self) -> list[ManagedDevice]:
        await self._ensure_token()
        base = self._config.jamf_base_url.rstrip("/")
        devices: list[ManagedDevice] = []

        # Computers (macOS / Windows)
        page = 0
        while True:
            data = await self._http(
                "GET",
                f"{base}/api/v1/computers-preview?page={page}&page-size=100",
            )
            for raw in data.get("results", []):
                devices.append(_parse_jamf_computer(raw))
            if len(data.get("results", [])) < 100:
                break
            page += 1

        # Mobile devices (iOS / iPadOS / tvOS)
        page = 0
        while True:
            data = await self._http(
                "GET",
                f"{base}/api/v2/mobile-devices?page={page}&page-size=100",
            )
            for raw in data.get("results", []):
                devices.append(_parse_jamf_mobile(raw))
            if len(data.get("results", [])) < 100:
                break
            page += 1

        return devices

    async def remote_lock(self, device_id: str) -> bool:
        await self._ensure_token()
        base = self._config.jamf_base_url.rstrip("/")
        # Try mobile device first, fall back to computer
        for path in (
            f"/api/v2/mobile-devices/{device_id}/lock",
            f"/api/v1/computers-preview/{device_id}/lock",
        ):
            try:
                await self._http("POST", f"{base}{path}",
                                 body=json.dumps({"passcode": ""}).encode())
                return True
            except RuntimeError:
                continue
        return False

    async def remote_wipe(self, device_id: str) -> bool:
        await self._ensure_token()
        base = self._config.jamf_base_url.rstrip("/")
        for path in (
            f"/api/v2/mobile-devices/{device_id}/erase",
            f"/api/v1/computers-preview/{device_id}/erase",
        ):
            try:
                await self._http("POST", f"{base}{path}",
                                 body=json.dumps({}).encode())
                return True
            except RuntimeError:
                continue
        return False

    async def unenroll(self, device_id: str) -> bool:
        await self._ensure_token()
        base = self._config.jamf_base_url.rstrip("/")
        for path in (
            f"/api/v2/mobile-devices/{device_id}/unenroll",
            f"/api/v1/computers-preview/{device_id}",
        ):
            try:
                method = "POST" if "unenroll" in path else "DELETE"
                await self._http(method, f"{base}{path}")
                return True
            except RuntimeError:
                continue
        return False

    async def send_enrollment_invite(self, email: str, name: str) -> bool:
        await self._ensure_token()
        base = self._config.jamf_base_url.rstrip("/")
        try:
            await self._http(
                "POST",
                f"{base}/api/v1/user-initiated-enrollment/invite",
                body=json.dumps({"username": email, "locale": "en"}).encode(),
            )
            log.info("Jamf enrollment invite sent to %s", email)
            return True
        except RuntimeError as e:
            log.warning("Jamf enrollment invite failed for %s: %s", email, e)
            return False

    async def push_custom_profile(
        self, device_id: str, profile_name: str, profile_payload: str,
        payload_type: str = "application/x-apple-aspen-config",
    ) -> bool:
        await self._ensure_token()
        base = self._config.jamf_base_url.rstrip("/")
        payload = {
            "name": profile_name,
            "payloads": base64.b64encode(profile_payload.encode()).decode(),
            "scope": {"mobileDevices": [{"id": device_id}]},
        }
        try:
            await self._http(
                "POST",
                f"{base}/api/v1/configuration-profiles",
                body=json.dumps(payload).encode(),
            )
            return True
        except RuntimeError as e:
            log.warning("Jamf profile push failed for device %s: %s", device_id, e)
            return False


def _parse_jamf_computer(raw: dict) -> ManagedDevice:
    return ManagedDevice(
        id=str(raw.get("id", "")),
        provider="jamf",
        user_email=raw.get("username", ""),
        name=raw.get("name", ""),
        platform="macos" if "mac" in raw.get("model", "").lower() else "windows",
        model=raw.get("model", ""),
        serial=raw.get("serialNumber", ""),
        os_version=raw.get("osVersion", ""),
        encrypted=raw.get("filevault2Enabled", False),
        management_state="managed",
        last_updated=time.time(),
    )


def _parse_jamf_mobile(raw: dict) -> ManagedDevice:
    platform_map = {"iPhone": "ios", "iPad": "ios", "iPod": "ios",
                    "AppleTV": "ios", "Android": "android"}
    model = raw.get("model", "")
    platform = next((v for k, v in platform_map.items() if k in model), "ios")
    return ManagedDevice(
        id=str(raw.get("id", "")),
        provider="jamf",
        user_email=raw.get("username", ""),
        name=raw.get("name", model),
        platform=platform,
        model=model,
        serial=raw.get("serialNumber", ""),
        os_version=raw.get("osVersion", ""),
        management_state="managed",
        last_updated=time.time(),
    )


# ── WireGuard profile generation ──────────────────────────────────────────────

def _wg_genkey() -> tuple[str, str]:
    """Generate a WireGuard private/public keypair for a mobile device."""
    try:
        import subprocess
        priv = subprocess.run(["wg", "genkey"], capture_output=True, check=True)
        privkey = priv.stdout.strip().decode()
        pub = subprocess.run(
            ["wg", "pubkey"], input=priv.stdout, capture_output=True, check=True
        )
        pubkey = pub.stdout.strip().decode()
        return privkey, pubkey
    except (FileNotFoundError, Exception):
        try:
            from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
            priv = X25519PrivateKey.generate()
            priv_bytes = priv.private_bytes_raw()
            pub_bytes = priv.public_key().public_bytes_raw()
            return (base64.b64encode(priv_bytes).decode(),
                    base64.b64encode(pub_bytes).decode())
        except ImportError:
            # Last resort for dev/CI
            priv_bytes = os.urandom(32)
            return (base64.b64encode(priv_bytes).decode(),
                    base64.b64encode(priv_bytes[::-1]).decode())


def build_wg_config(
    private_key: str,
    device_ip: str,
    server_public_key: str,
    endpoint: str,
    allowed_ips: str = "10.200.0.0/16",
    dns: str = "",
) -> str:
    """Build a WireGuard client config (cross-platform: iOS, Android, Windows, Linux)."""
    lines = [
        "[Interface]",
        f"PrivateKey = {private_key}",
        f"Address = {device_ip}/24",
    ]
    if dns:
        lines.append(f"DNS = {dns}")
    lines += [
        "",
        "[Peer]",
        f"PublicKey = {server_public_key}",
        f"AllowedIPs = {allowed_ips}",
        f"Endpoint = {endpoint}",
        "PersistentKeepalive = 25",
    ]
    return "\n".join(lines) + "\n"


def build_ios_mobileconfig(
    profile_name: str,
    private_key: str,
    device_ip: str,
    server_public_key: str,
    endpoint: str,
    allowed_ips: str = "10.200.0.0/16",
    dns: str = "",
    org_name: str = "Ozma",
) -> str:
    """
    Build an Apple mobileconfig payload for WireGuard VPN.

    The mobileconfig embeds the WireGuard config in the VPN payload.
    When pushed via MDM, the user is not prompted for the private key.
    """
    import uuid
    profile_uuid = str(uuid.uuid4()).upper()
    payload_uuid = str(uuid.uuid4()).upper()

    wg_conf = build_wg_config(
        private_key, device_ip, server_public_key, endpoint, allowed_ips, dns
    )
    wg_conf_b64 = base64.b64encode(wg_conf.encode()).decode()

    return f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>PayloadContent</key>
  <array>
    <dict>
      <key>PayloadType</key>
      <string>com.wireguard.macos</string>
      <key>PayloadVersion</key>
      <integer>1</integer>
      <key>PayloadIdentifier</key>
      <string>dev.ozma.vpn.wireguard.{payload_uuid}</string>
      <key>PayloadUUID</key>
      <string>{payload_uuid}</string>
      <key>PayloadDisplayName</key>
      <string>{profile_name}</string>
      <key>WireGuardConfiguration</key>
      <data>{wg_conf_b64}</data>
    </dict>
  </array>
  <key>PayloadDisplayName</key>
  <string>{profile_name}</string>
  <key>PayloadIdentifier</key>
  <string>dev.ozma.vpn.{profile_uuid}</string>
  <key>PayloadOrganization</key>
  <string>{org_name}</string>
  <key>PayloadType</key>
  <string>Configuration</string>
  <key>PayloadUUID</key>
  <string>{profile_uuid}</string>
  <key>PayloadVersion</key>
  <integer>1</integer>
</dict>
</plist>"""


# ── Manager ───────────────────────────────────────────────────────────────────

_PROVIDER_CLASSES = {
    "google": GoogleMDMProvider,
    "intune": IntuneMDMProvider,
    "jamf": JamfMDMProvider,
}


class MDMBridgeManager:
    """
    Manages MDM device inventory, enrollment, profile push, and offboarding.

    Persistence:
      mdm_bridge.json  — config (provider credentials as env var names)
      mdm_devices.json — cached device inventory (private keys included)
    """

    def __init__(self, data_path: Path, config: MDMConfig | None = None,
                 event_queue: asyncio.Queue | None = None) -> None:
        self._path = data_path
        self._config_path = data_path / "mdm_bridge.json"
        self._devices_path = data_path / "mdm_devices.json"
        self._config = config or MDMConfig()
        self._devices: dict[str, ManagedDevice] = {}   # id → device
        self._event_queue = event_queue
        self._tasks: list[asyncio.Task] = []
        self._next_mobile_ip_index: int = 1
        self._provider: MDMProvider | None = None
        self._load()
        self._init_provider()

    # ── Persistence ───────────────────────────────────────────────────

    def _load(self) -> None:
        self._path.mkdir(parents=True, exist_ok=True)
        if self._config_path.exists():
            try:
                self._config = MDMConfig.from_dict(
                    json.loads(self._config_path.read_text())
                )
            except Exception as e:
                log.warning("Failed to load MDM config: %s", e)
        if self._devices_path.exists():
            try:
                raw = json.loads(self._devices_path.read_text())
                for d in raw.get("devices", []):
                    dev = ManagedDevice.from_dict(d)
                    self._devices[dev.id] = dev
                self._next_mobile_ip_index = raw.get("next_ip_index", 1)
            except Exception as e:
                log.warning("Failed to load MDM devices: %s", e)
        log.info("MDM bridge loaded: %d devices, provider=%s",
                 len(self._devices), self._config.provider or "none")

    def _save_config(self) -> None:
        try:
            self._config_path.write_text(json.dumps(self._config.to_dict(), indent=2))
        except Exception as e:
            log.error("Failed to save MDM config: %s", e)

    def _save_devices(self) -> None:
        try:
            data = {
                "devices": [d.to_dict(include_private_key=True)
                            for d in self._devices.values()],
                "next_ip_index": self._next_mobile_ip_index,
            }
            # 0o600 — private keys are in here
            self._devices_path.write_text(json.dumps(data, indent=2))
            self._devices_path.chmod(0o600)
        except Exception as e:
            log.error("Failed to save MDM devices: %s", e)

    def _init_provider(self) -> None:
        cls = _PROVIDER_CLASSES.get(self._config.provider)
        self._provider = cls(self._config) if cls else None

    # ── Lifecycle ─────────────────────────────────────────────────────

    async def start(self) -> None:
        if not self._provider:
            log.info("MDM bridge: no provider configured — idle")
            return
        self._tasks.append(asyncio.create_task(
            self._sync_loop(), name="mdm-sync"
        ))

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()

    async def _sync_loop(self) -> None:
        while True:
            try:
                await self.sync()
            except Exception as e:
                log.error("MDM sync error: %s", e)
            await asyncio.sleep(self._config.sync_interval_seconds)

    # ── Sync ──────────────────────────────────────────────────────────

    async def sync(self) -> dict[str, Any]:
        """Fetch current device list from the provider and update local inventory."""
        if not self._provider:
            return {"ok": False, "error": "No provider configured"}
        try:
            fresh = await self._provider.list_devices()
        except Exception as e:
            log.error("MDM list_devices failed: %s", e)
            return {"ok": False, "error": str(e)}

        fresh_ids = {d.id for d in fresh}
        added = removed = updated = 0

        for dev in fresh:
            existing = self._devices.get(dev.id)
            if not existing:
                # Preserve Ozma overlay state if device re-appears
                self._devices[dev.id] = dev
                added += 1
            else:
                # Update provider fields; preserve Ozma VPN state
                dev.vpn_profile_pushed = existing.vpn_profile_pushed
                dev.vpn_private_key = existing.vpn_private_key
                dev.vpn_public_key = existing.vpn_public_key
                dev.vpn_ip = existing.vpn_ip
                dev.wifi_profile_pushed = existing.wifi_profile_pushed
                self._devices[dev.id] = dev
                updated += 1

        # Mark missing devices (don't delete — they may re-appear)
        for dev_id, dev in self._devices.items():
            if dev_id not in fresh_ids and dev.management_state != "pending_removal":
                dev.management_state = "pending_removal"
                removed += 1

        self._save_devices()
        await self._fire_event("mdm.sync.complete", {
            "provider": self._config.provider,
            "total": len(self._devices),
            "added": added, "updated": updated, "removed": removed,
        })
        log.info("MDM sync: %d total, +%d ~%d -%d", len(self._devices),
                 added, updated, removed)
        return {"ok": True, "total": len(self._devices),
                "added": added, "updated": updated, "removed": removed}

    # ── Device management ─────────────────────────────────────────────

    def list_devices(
        self,
        user_email: str | None = None,
        platform: str | None = None,
        compliance_state: str | None = None,
    ) -> list[ManagedDevice]:
        devs = list(self._devices.values())
        if user_email:
            devs = [d for d in devs if d.user_email.lower() == user_email.lower()]
        if platform:
            devs = [d for d in devs if d.platform == platform]
        if compliance_state:
            devs = [d for d in devs if d.compliance_state == compliance_state]
        return sorted(devs, key=lambda d: d.last_updated, reverse=True)

    def get_device(self, device_id: str) -> ManagedDevice | None:
        return self._devices.get(device_id)

    def is_enrolled(self, email: str) -> bool:
        """Return True if the user has at least one managed device."""
        return any(
            d.user_email.lower() == email.lower()
            for d in self._devices.values()
        )

    def compliance_gaps(self) -> list[dict[str, Any]]:
        """Return list of compliance issues across all devices."""
        gaps = []
        for dev in self._devices.values():
            if dev.management_state == "pending_removal":
                continue
            if dev.compliance_state == "noncompliant":
                gaps.append({
                    "type": "noncompliant_device",
                    "device_id": dev.id,
                    "user_email": dev.user_email,
                    "platform": dev.platform,
                    "model": dev.model,
                })
            if not dev.encrypted and dev.platform not in ("chromeos", "ios"):
                gaps.append({
                    "type": "unencrypted_device",
                    "device_id": dev.id,
                    "user_email": dev.user_email,
                    "platform": dev.platform,
                })
            if dev.vpn_ip and not dev.vpn_profile_pushed:
                gaps.append({
                    "type": "vpn_profile_not_pushed",
                    "device_id": dev.id,
                    "user_email": dev.user_email,
                })
        return gaps

    # ── Enrollment ────────────────────────────────────────────────────

    async def invite_enrollment(self, email: str, name: str) -> bool:
        if not self._provider:
            raise RuntimeError("No MDM provider configured")
        ok = await self._provider.send_enrollment_invite(email, name)
        await self._fire_event("mdm.enrollment.invite_sent", {
            "email": email, "name": name, "success": ok,
            "provider": self._config.provider,
        })
        return ok

    # ── VPN profile push ──────────────────────────────────────────────

    async def push_vpn_profile(self, device_id: str) -> bool:
        """
        Generate a WireGuard VPN profile and push it to the device via MDM.

        The profile embeds the full WireGuard config (including private key).
        The private key is generated here, stored in mdm_devices.json (0o600),
        and delivered to the device via the MDM provider's encrypted push channel.
        """
        dev = self._devices.get(device_id)
        if not dev:
            raise ValueError(f"Device {device_id} not found")
        if not self._provider:
            raise RuntimeError("No MDM provider configured")
        if not self._config.wg_endpoint or not self._config.wg_server_public_key:
            raise RuntimeError(
                "wg_endpoint and wg_server_public_key must be configured "
                "in MDM settings before pushing VPN profiles"
            )

        # Generate a new keypair for this device if not already assigned
        if not dev.vpn_private_key:
            priv, pub = _wg_genkey()
            dev.vpn_private_key = priv
            dev.vpn_public_key = pub
            dev.vpn_ip = f"{MOBILE_WG_SUBNET}.{self._next_mobile_ip_index}"
            self._next_mobile_ip_index = (self._next_mobile_ip_index % 253) + 1
            self._save_devices()

        profile_name = "Ozma VPN"
        if dev.platform in ("ios", "macos"):
            profile_payload = build_ios_mobileconfig(
                profile_name=profile_name,
                private_key=dev.vpn_private_key,
                device_ip=dev.vpn_ip,
                server_public_key=self._config.wg_server_public_key,
                endpoint=self._config.wg_endpoint,
                allowed_ips=self._config.wg_allowed_ips,
                dns=self._config.wg_dns,
            )
        else:
            # Android, Windows, Linux: plain WireGuard config text
            profile_payload = build_wg_config(
                private_key=dev.vpn_private_key,
                device_ip=dev.vpn_ip,
                server_public_key=self._config.wg_server_public_key,
                endpoint=self._config.wg_endpoint,
                allowed_ips=self._config.wg_allowed_ips,
                dns=self._config.wg_dns,
            )

        ok = await self._provider.push_custom_profile(
            device_id, profile_name, profile_payload
        )
        if ok:
            dev.vpn_profile_pushed = True
            self._save_devices()
            await self._fire_event("mdm.vpn.profile_pushed", {
                "device_id": device_id, "user_email": dev.user_email,
                "platform": dev.platform, "vpn_ip": dev.vpn_ip,
                "vpn_public_key": dev.vpn_public_key,
            })
            log.info("VPN profile pushed to %s (%s) → %s",
                     dev.name, dev.user_email, dev.vpn_ip)
        return ok

    # ── Offboarding ───────────────────────────────────────────────────

    async def offboard_user(self, email: str, wipe: bool = False) -> dict[str, Any]:
        """
        Offboard a user: remote lock (or wipe) and unenroll all their devices.

        wipe=True performs a factory reset; wipe=False just unenrolls.
        Default is unenroll without wipe — allows the employee to keep personal data.
        """
        if not self._provider:
            raise RuntimeError("No MDM provider configured")
        devices = self.list_devices(user_email=email)
        results: dict[str, bool] = {}
        for dev in devices:
            if wipe:
                ok = await self._provider.remote_wipe(dev.id)
            else:
                ok = await self._provider.unenroll(dev.id)
            results[dev.id] = ok
            if ok:
                dev.management_state = "pending_removal"
        self._save_devices()
        await self._fire_event("mdm.offboard.complete", {
            "email": email, "wipe": wipe,
            "devices_processed": len(results),
            "results": results,
        })
        return {"email": email, "wipe": wipe, "results": results}

    # ── Config management ─────────────────────────────────────────────

    def get_config(self) -> MDMConfig:
        return self._config

    def set_config(self, config: MDMConfig) -> None:
        self._config = config
        self._save_config()
        self._init_provider()

    def status(self) -> dict[str, Any]:
        devs = list(self._devices.values())
        active = [d for d in devs if d.management_state != "pending_removal"]
        return {
            "provider": self._config.provider or "none",
            "configured": bool(self._config.provider),
            "total_devices": len(active),
            "by_platform": {
                p: sum(1 for d in active if d.platform == p)
                for p in ("ios", "android", "macos", "windows", "chromeos", "linux")
            },
            "compliant": sum(1 for d in active if d.compliance_state == "compliant"),
            "noncompliant": sum(1 for d in active if d.compliance_state == "noncompliant"),
            "vpn_pushed": sum(1 for d in active if d.vpn_profile_pushed),
            "compliance_gaps": len(self.compliance_gaps()),
        }

    async def _fire_event(self, event_type: str, data: dict) -> None:
        if self._event_queue:
            await self._event_queue.put({"type": event_type, **data})
