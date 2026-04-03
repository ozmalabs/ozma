# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
from dataclasses import dataclass, field
import os


@dataclass
class Config:
    # API server
    api_host: str = "0.0.0.0"
    api_port: int = 7380

    # Node communication
    node_port: int = 7331

    # mDNS
    mdns_service_type: str = "_ozma._udp.local."
    mdns_requery_interval: float = 15.0  # seconds — picks up late TXT records on busy hosts

    # Input capture
    # If None, controller will scan /dev/input for keyboard/mouse
    keyboard_device: str | None = None
    mouse_device: str | None = None
    # Only capture devices whose name starts with "ozma-virtual-" (test harness mode)
    virtual_only: bool = False

    # Audio routing (V0.3)
    audio_enabled: bool = True
    audio_output_sink: str = ""      # PW sink name; empty = use default
    audio_mic_source: str = ""       # PW source name; empty = use default
    audio_wireplumber: bool = False  # Use WirePlumber metadata mode (requires ozma-routing.lua)

    # Control surfaces
    controls_config: str = ""        # Path to controls.yaml; empty = built-in defaults only

    # Authentication
    # Off by default — enable with OZMA_AUTH=1 once the dashboard has a login flow.
    # When off, the API is open (same as before). When on, JWT required.
    auth_enabled: bool = False
    auth_password_hash: str = ""   # Argon2id hash; set via OZMA_AUTH_PASSWORD env

    # Identity Provider — built-in OIDC/social login.  Requires auth_enabled.
    idp_enabled: bool = False

    # Hardware front panel — I2C OLED + GPIO buttons on appliance builds
    front_panel_enabled: bool = False

    # A/B partition update manager — only meaningful on bare-metal appliances
    update_manager_enabled: bool = False

    # Live transcription via Whisper.cpp
    transcription_enabled: bool = False
    transcription_source: str = ""   # PipeWire source name; empty = default mic

    # Logging
    debug: bool = False

    @classmethod
    def from_env(cls) -> "Config":
        return cls(
            api_host=os.environ.get("OZMA_API_HOST", "0.0.0.0"),
            api_port=int(os.environ.get("OZMA_API_PORT", "7380")),
            node_port=int(os.environ.get("OZMA_NODE_PORT", "7331")),
            keyboard_device=os.environ.get("OZMA_KBD_DEVICE"),
            mouse_device=os.environ.get("OZMA_MOUSE_DEVICE"),
            virtual_only=os.environ.get("OZMA_VIRTUAL_ONLY", "").lower() in ("1", "true", "yes"),
            debug=os.environ.get("OZMA_DEBUG", "").lower() in ("1", "true", "yes"),
            audio_enabled=os.environ.get("OZMA_AUDIO", "1").lower() not in ("0", "false", "no"),
            audio_output_sink=os.environ.get("OZMA_AUDIO_OUTPUT", ""),
            audio_mic_source=os.environ.get("OZMA_AUDIO_MIC", ""),
            audio_wireplumber=os.environ.get("OZMA_AUDIO_WIREPLUMBER", "").lower() in ("1", "true", "yes"),
            controls_config=os.environ.get("OZMA_CONTROLS_CONFIG", ""),
            auth_enabled=os.environ.get("OZMA_AUTH", "0").lower() in ("1", "true", "yes"),
            auth_password_hash=os.environ.get("OZMA_AUTH_PASSWORD_HASH", ""),
            idp_enabled=os.environ.get("OZMA_IDP", "0").lower() in ("1", "true", "yes"),
            front_panel_enabled=os.environ.get("OZMA_FRONT_PANEL", "0").lower() in ("1", "true", "yes"),
            update_manager_enabled=os.environ.get("OZMA_UPDATE_MANAGER", "0").lower() in ("1", "true", "yes"),
            transcription_enabled=os.environ.get("OZMA_TRANSCRIPTION", "0").lower() in ("1", "true", "yes"),
            transcription_source=os.environ.get("OZMA_TRANSCRIPTION_SOURCE", ""),
        )
