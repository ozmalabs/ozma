#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Camera advisor — snapshot + AI analysis → name/zone/trigger suggestions.

Workflow:
  1. Fetch a JPEG snapshot from the camera (Frigate /latest.jpg or CameraManager)
  2. Send to a vision-capable LLM with a structured prompt
  3. Parse the JSON response (scene, detected objects, suggested zones)
  4. Build three ready-to-use Frigate config profiles:
       default   — sensible defaults for a home/office camera
       paranoid  — maximum coverage, tight thresholds, long retention
       lax       — minimal noise, relaxed thresholds, shorter retention
  5. Return CameraAdvice with all three profiles + AI reasoning

The caller chooses a profile and can apply it (rename camera, push Frigate config).

Backend configuration (environment variables):

  OZMA_CAMERA_ADVISOR_BACKEND   anthropic (default) | ollama | openai
  OZMA_CAMERA_ADVISOR_MODEL     model name; defaults per backend:
                                  anthropic → claude-sonnet-4-6
                                  ollama    → llava
                                  openai    → gpt-4o
  OZMA_CAMERA_ADVISOR_URL       base URL for ollama/openai backends
                                  ollama    → http://localhost:11434
                                  openai    → https://api.openai.com
                                  (any OpenAI-compatible server works here —
                                   LM Studio, vLLM, llama.cpp, etc.)
  ANTHROPIC_API_KEY             required for anthropic backend
  OPENAI_API_KEY                required for openai backend (not ollama)

The advisor always returns something — if the LLM call fails it falls back to a
safe heuristic profile so camera setup is never blocked.

Speed note: local models (ollama) are slower but free and fully private.
Camera advice is a one-time setup step, so latency is not a concern.
"""

from __future__ import annotations

import base64
import json
import logging
import os
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.camera_advisor")

# ── Data model ───────────────────────────────────────────────────────────────

@dataclass
class Zone:
    """A suggested detection zone in normalised coordinates (0.0–1.0)."""
    name: str
    description: str
    # Polygon as flat list of x,y pairs: [x0,y0, x1,y1, ...]
    coordinates: list[float] = field(default_factory=list)
    objects: list[str] = field(default_factory=list)  # objects to track in this zone

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "description": self.description,
            "coordinates": self.coordinates,
            "objects": self.objects,
        }

    def frigate_coord_str(self) -> str:
        """Format coordinates for Frigate config (comma-separated x,y pairs)."""
        return ",".join(str(round(v, 4)) for v in self.coordinates)


@dataclass
class FrigateProfile:
    """A complete Frigate configuration profile for one camera."""
    label: str                          # Human-readable: "Sensible default"
    description: str                    # One-line explanation of the tradeoffs
    objects: list[str]                  # Objects to track
    min_score: float                    # Detection confidence threshold
    threshold: float                    # Post-NMS track threshold
    zones: list[Zone]                   # Zone definitions
    record_retain_days: int             # Days to keep continuous recordings
    event_retain_days: int              # Days to keep event clips
    snapshots_retain_days: int          # Days to keep snapshots
    motion_threshold: int               # Motion sensitivity (lower = more sensitive)
    always_on_recording: bool = False   # Record continuously (not just on motion)
    alert_on_loiter: bool = False       # Alert when object lingers in zone
    loiter_seconds: int = 30            # How long before a loiter alert fires
    min_area: int = 100                 # Minimum object bounding box area (px²)

    def to_dict(self) -> dict:
        return {
            "label": self.label,
            "description": self.description,
            "objects": self.objects,
            "min_score": self.min_score,
            "threshold": self.threshold,
            "zones": [z.to_dict() for z in self.zones],
            "record_retain_days": self.record_retain_days,
            "event_retain_days": self.event_retain_days,
            "snapshots_retain_days": self.snapshots_retain_days,
            "motion_threshold": self.motion_threshold,
            "always_on_recording": self.always_on_recording,
            "alert_on_loiter": self.alert_on_loiter,
            "loiter_seconds": self.loiter_seconds,
            "min_area": self.min_area,
        }

    def to_frigate_yaml(self, camera_id: str) -> str:
        """
        Return a Frigate camera config block as YAML text.
        The caller pastes or merges this into their frigate.yml.
        """
        lines = [f"  {camera_id}:"]
        lines.append(f"    objects:")
        lines.append(f"      track:")
        for obj in self.objects:
            lines.append(f"        - {obj}")
        lines.append(f"      filters:")
        for obj in self.objects:
            lines.append(f"        {obj}:")
            lines.append(f"          min_score: {self.min_score}")
            lines.append(f"          threshold: {self.threshold}")
            lines.append(f"          min_area: {self.min_area}")

        if self.zones:
            lines.append(f"    zones:")
            for z in self.zones:
                lines.append(f"      {z.name}:")
                lines.append(f"        coordinates: {z.frigate_coord_str()}")
                if z.objects:
                    lines.append(f"        objects:")
                    for obj in z.objects:
                        lines.append(f"          - {obj}")

        lines.append(f"    record:")
        lines.append(f"      enabled: true")
        lines.append(f"      retain:")
        lines.append(f"        days: {self.record_retain_days}")
        lines.append(f"        mode: {'all' if self.always_on_recording else 'motion'}")
        lines.append(f"      events:")
        lines.append(f"        retain:")
        lines.append(f"          default: {self.event_retain_days}")
        lines.append(f"          mode: active_objects")

        lines.append(f"    snapshots:")
        lines.append(f"      enabled: true")
        lines.append(f"      retain:")
        lines.append(f"        default: {self.snapshots_retain_days}")

        lines.append(f"    motion:")
        lines.append(f"      threshold: {self.motion_threshold}")

        return "\n".join(lines)


@dataclass
class CameraAdvice:
    """Full advice package returned to the caller."""
    camera_id: str
    suggested_name: str             # e.g. "Front Door"
    scene_description: str          # What Claude saw
    detected_objects: list[str]     # Object types visible in the scene
    ai_reasoning: str               # Why these suggestions were made
    profiles: dict[str, FrigateProfile]   # "default", "paranoid", "lax"
    snapshot_b64: str = ""          # Base64 JPEG used for analysis (for UI preview)
    error: str = ""                 # Non-empty if AI call failed (heuristic used)

    def to_dict(self) -> dict:
        return {
            "camera_id": self.camera_id,
            "suggested_name": self.suggested_name,
            "scene_description": self.scene_description,
            "detected_objects": self.detected_objects,
            "ai_reasoning": self.ai_reasoning,
            "profiles": {k: v.to_dict() for k, v in self.profiles.items()},
            "snapshot_b64": self.snapshot_b64,
            "error": self.error,
        }


# ── Vision prompt ─────────────────────────────────────────────────────────────

_SYSTEM_PROMPT = """\
You are an expert home security camera configuration assistant.
Analyse the provided camera snapshot and return ONLY a valid JSON object — no markdown fences, no prose.

The JSON schema:
{
  "name": "<short descriptive name, 2-3 words, e.g. 'Front Door'>",
  "scene": "<one sentence describing what the camera sees>",
  "objects": ["<object type>", ...],   // objects a security camera should track here
  "zones": [
    {
      "name": "<slug, e.g. front_porch>",
      "description": "<what this zone covers>",
      "coordinates": [x0,y0, x1,y1, x2,y2, x3,y3],  // normalised 0.0-1.0 polygon (clockwise)
      "objects": ["<object type>", ...]               // objects relevant to this zone
    }
  ],
  "reasoning": "<brief explanation of your suggestions>"
}

Rules:
- objects: choose from Frigate's built-in labels: person, car, dog, cat, bird, bicycle, motorcycle, bus, truck, deer, package
- zones: suggest 1-3 zones that make sense given the scene geometry (entry points, driveway, yard, etc.)
- If you can't identify meaningful zones from the image, return an empty zones array
- Coordinates are normalised (0.0 = left/top, 1.0 = right/bottom)
- name: be specific — "Back Gate", "Driveway Entrance", "Side Alley", not just "Camera"
"""

_USER_PROMPT = "Analyse this camera snapshot and return the JSON configuration advice."


# ── Backend configuration ─────────────────────────────────────────────────────

_DEFAULT_MODELS = {
    "anthropic": "claude-sonnet-4-6",
    "ollama":    "llava",
    "openai":    "gpt-4o",
}

_DEFAULT_URLS = {
    "ollama": "http://localhost:11434",
    "openai": "https://api.openai.com",
}


def _advisor_config() -> tuple[str, str, str]:
    """Return (backend, model, base_url) from environment."""
    backend = os.environ.get("OZMA_CAMERA_ADVISOR_BACKEND", "anthropic").lower()
    if backend not in _DEFAULT_MODELS:
        raise RuntimeError(
            f"Unknown OZMA_CAMERA_ADVISOR_BACKEND={backend!r}. "
            f"Choose: {', '.join(_DEFAULT_MODELS)}"
        )
    model = os.environ.get("OZMA_CAMERA_ADVISOR_MODEL") or _DEFAULT_MODELS[backend]
    url   = os.environ.get("OZMA_CAMERA_ADVISOR_URL")   or _DEFAULT_URLS.get(backend, "")
    return backend, model, url


# ── JSON extraction ───────────────────────────────────────────────────────────

def _extract_json(text: str) -> dict:
    """Parse JSON from LLM output, stripping any markdown fences."""
    text = text.strip()
    if text.startswith("```"):
        # ```json ... ``` or ``` ... ```
        inner = text.split("```", 2)
        text = inner[1]
        if text.startswith("json"):
            text = text[4:]
        text = text.strip()
    return json.loads(text)


# ── Backend implementations ───────────────────────────────────────────────────

async def _call_anthropic(jpeg_bytes: bytes, model: str) -> dict:
    api_key = os.environ.get("ANTHROPIC_API_KEY", "")
    if not api_key:
        raise RuntimeError("ANTHROPIC_API_KEY not set")

    b64 = base64.standard_b64encode(jpeg_bytes).decode()
    payload = {
        "model": model,
        "max_tokens": 1024,
        "system": _SYSTEM_PROMPT,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {"type": "base64", "media_type": "image/jpeg", "data": b64},
                },
                {"type": "text", "text": _USER_PROMPT},
            ],
        }],
    }

    import aiohttp
    async with aiohttp.ClientSession() as session:
        async with session.post(
            "https://api.anthropic.com/v1/messages",
            json=payload,
            headers={
                "x-api-key": api_key,
                "anthropic-version": "2023-06-01",
                "content-type": "application/json",
            },
            timeout=aiohttp.ClientTimeout(total=60),
        ) as resp:
            if resp.status != 200:
                raise RuntimeError(f"Anthropic API {resp.status}: {(await resp.text())[:200]}")
            data = await resp.json()

    return _extract_json(data["content"][0]["text"])


async def _call_ollama(jpeg_bytes: bytes, model: str, base_url: str) -> dict:
    """
    Ollama native chat API with vision support.

    Uses POST {base_url}/api/chat with the `images` field (base64 JPEG list).
    Compatible with any Ollama-hosted vision model: llava, llava:13b,
    llava:34b, bakllava, moondream, minicpm-v, etc.

    Ollama has no timeout by default — we use 300s since local models
    can be slow on CPU but the analysis is a one-time setup step.
    """
    b64 = base64.standard_b64encode(jpeg_bytes).decode()

    # Combine system + user prompt: Ollama supports system role in messages
    payload = {
        "model": model,
        "stream": False,
        "messages": [
            {"role": "system", "content": _SYSTEM_PROMPT},
            {
                "role": "user",
                "content": _USER_PROMPT,
                "images": [b64],
            },
        ],
    }

    import aiohttp
    async with aiohttp.ClientSession() as session:
        async with session.post(
            f"{base_url.rstrip('/')}/api/chat",
            json=payload,
            timeout=aiohttp.ClientTimeout(total=300),
        ) as resp:
            if resp.status != 200:
                raise RuntimeError(f"Ollama API {resp.status}: {(await resp.text())[:200]}")
            data = await resp.json()

    text = data["message"]["content"]
    return _extract_json(text)


async def _call_openai_compat(jpeg_bytes: bytes, model: str, base_url: str) -> dict:
    """
    OpenAI-compatible chat/completions endpoint with vision.

    Works with:
      - OpenAI (gpt-4o, gpt-4-turbo-vision)
      - LM Studio  (http://localhost:1234)
      - vLLM       (http://localhost:8000)
      - llama.cpp  (http://localhost:8080)
      - any server implementing POST /v1/chat/completions with image_url support
    """
    api_key = os.environ.get("OPENAI_API_KEY", "none")  # some local servers ignore the key
    b64 = base64.standard_b64encode(jpeg_bytes).decode()
    data_url = f"data:image/jpeg;base64,{b64}"

    payload = {
        "model": model,
        "max_tokens": 1024,
        "messages": [
            {"role": "system", "content": _SYSTEM_PROMPT},
            {
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": data_url}},
                    {"type": "text",      "text": _USER_PROMPT},
                ],
            },
        ],
    }

    import aiohttp
    async with aiohttp.ClientSession() as session:
        async with session.post(
            f"{base_url.rstrip('/')}/v1/chat/completions",
            json=payload,
            headers={"Authorization": f"Bearer {api_key}"},
            timeout=aiohttp.ClientTimeout(total=300),
        ) as resp:
            if resp.status != 200:
                raise RuntimeError(f"OpenAI-compat API {resp.status}: {(await resp.text())[:200]}")
            data = await resp.json()

    text = data["choices"][0]["message"]["content"]
    return _extract_json(text)


# ── Dispatcher ────────────────────────────────────────────────────────────────

async def _call_llm(jpeg_bytes: bytes) -> dict:
    """Route to the configured backend and return the parsed JSON dict."""
    backend, model, url = _advisor_config()
    log.info("Camera advisor: backend=%s model=%s", backend, model)
    match backend:
        case "anthropic":
            return await _call_anthropic(jpeg_bytes, model)
        case "ollama":
            return await _call_ollama(jpeg_bytes, model, url)
        case "openai":
            return await _call_openai_compat(jpeg_bytes, model, url)


# ── Heuristic fallback ────────────────────────────────────────────────────────

def _heuristic_advice(camera_id: str) -> dict:
    """
    Generic advice when no AI is available.
    Returns a minimal but safe suggestion with a whole-frame zone.
    """
    return {
        "name": camera_id.replace("_", " ").replace("-", " ").title(),
        "scene": "Unknown scene — AI analysis unavailable",
        "objects": ["person", "car"],
        "zones": [
            {
                "name": "main_area",
                "description": "Full frame (adjust after reviewing footage)",
                "coordinates": [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0],
                "objects": ["person", "car"],
            }
        ],
        "reasoning": (
            "Heuristic defaults — configure OZMA_CAMERA_ADVISOR_BACKEND "
            "(anthropic/ollama/openai) for AI-powered advice."
        ),
    }


# ── Profile builder ───────────────────────────────────────────────────────────

def _build_profiles(ai: dict) -> dict[str, FrigateProfile]:
    """Convert AI analysis dict into three FrigateProfile instances."""
    objects_all = ai.get("objects", ["person", "car"])
    objects_person_only = ["person"]
    zones = [
        Zone(
            name=z["name"],
            description=z.get("description", ""),
            coordinates=z.get("coordinates", [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]),
            objects=z.get("objects", objects_all),
        )
        for z in ai.get("zones", [])
    ]

    return {
        "default": FrigateProfile(
            label="Sensible default",
            description=(
                "Tracks people and vehicles. Alerts on zone entry. "
                "14-day clip retention. Good balance of coverage and noise."
            ),
            objects=["person", "car", "dog"] if "dog" in objects_all else ["person", "car"],
            min_score=0.60,
            threshold=0.70,
            zones=zones,
            record_retain_days=14,
            event_retain_days=14,
            snapshots_retain_days=14,
            motion_threshold=25,
            always_on_recording=False,
            alert_on_loiter=False,
            loiter_seconds=30,
            min_area=500,
        ),
        "paranoid": FrigateProfile(
            label="Paranoid",
            description=(
                "Tracks everything detected in the scene. Tight thresholds, "
                "always-on recording, 30-day retention. Expect more alerts."
            ),
            objects=objects_all,
            min_score=0.50,
            threshold=0.60,
            zones=zones,
            record_retain_days=30,
            event_retain_days=30,
            snapshots_retain_days=30,
            motion_threshold=15,
            always_on_recording=True,
            alert_on_loiter=True,
            loiter_seconds=10,
            min_area=100,
        ),
        "lax": FrigateProfile(
            label="Lax",
            description=(
                "People only. Higher confidence threshold to reduce false positives. "
                "7-day retention. Minimal noise — only confident detections alert."
            ),
            objects=objects_person_only,
            min_score=0.75,
            threshold=0.85,
            zones=zones,
            record_retain_days=7,
            event_retain_days=7,
            snapshots_retain_days=7,
            motion_threshold=40,
            always_on_recording=False,
            alert_on_loiter=False,
            loiter_seconds=60,
            min_area=1000,
        ),
    }


# ── Public API ────────────────────────────────────────────────────────────────

async def advise_camera(
    camera_id: str,
    jpeg_bytes: bytes,
) -> CameraAdvice:
    """
    Analyse a JPEG snapshot and return AI-generated camera advice.

    Args:
        camera_id:   ID of the camera being configured (used for naming fallbacks)
        jpeg_bytes:  Raw JPEG image bytes (from CameraManager.snapshot or Frigate)

    Returns:
        CameraAdvice with suggested name, scene description, detected objects,
        and three Frigate config profiles (default / paranoid / lax).
    """
    b64 = base64.standard_b64encode(jpeg_bytes).decode()
    error = ""

    try:
        ai = await _call_llm(jpeg_bytes)
        log.info("Camera advisor: AI analysis succeeded for %s", camera_id)
    except Exception as exc:
        log.warning("Camera advisor: AI call failed for %s: %s — using heuristic", camera_id, exc)
        ai = _heuristic_advice(camera_id)
        error = str(exc)

    profiles = _build_profiles(ai)

    return CameraAdvice(
        camera_id=camera_id,
        suggested_name=ai.get("name", camera_id),
        scene_description=ai.get("scene", ""),
        detected_objects=ai.get("objects", []),
        ai_reasoning=ai.get("reasoning", ""),
        profiles=profiles,
        snapshot_b64=b64,
        error=error,
    )
