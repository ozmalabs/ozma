# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Typed, bounded pass-through for meeting-detection status.

The controller does not scan processes or read calendars. It reads the
supervisor's ``meeting`` report (``GET /status``) and exposes a stable,
whitelisted document. This module is intentionally pure: no network calls, no
filesystem access, and no dependency on supervisor internals.

Safety rules:

* only known fields survive, so socket paths, binary paths, calendar UIDs and
  tokens can never leak through an unexpected upstream key;
* a meeting is ``active`` only when at least one real meeting entry is marked
  active; an inconsistent upstream report is downgraded to ``unknown``;
* text, lists and error messages are bounded, and error text is path/token
  redacted.
"""

from __future__ import annotations

import re
from typing import Any

MEETING_DETECT_STATES = frozenset(
    {"disabled", "starting", "running", "unavailable", "stopped"}
)
MEETING_STATES = frozenset({"idle", "active", "unknown"})
MEETING_PLATFORMS = frozenset(
    {"zoom", "teams", "meet", "slack", "discord", "facetime", "unknown"}
)
MEETING_ERROR_CODES = frozenset(
    {
        "binary_missing",
        "socket_missing",
        "socket_unavailable",
        "timeout",
        "protocol",
        "spawn_failed",
        "exited",
        "inconsistent_snapshot",
        "io",
        "unknown",
    }
)

MAX_MEETINGS = 8
MAX_TITLE_CHARS = 200
MAX_ORGANIZER_CHARS = 128
MAX_ID_CHARS = 64
MAX_ERROR_CHARS = 200

_PATH_RE = re.compile(r"(?<![\w.])/(?:[^\s\"']+)")
_BEARER_RE = re.compile(r"(?i)bearer\s+\S+")


def _bounded_text(value: object, limit: int) -> str:
    text = value if isinstance(value, str) else ""
    text = "".join(ch if ch.isprintable() else " " for ch in text)
    text = text.strip()
    if len(text) > limit:
        text = text[:limit]
    return text


def _bounded_int(value: object) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int):
        return None
    return max(0, value)


def _redact_error_message(value: object) -> str:
    text = _bounded_text(value, MAX_ERROR_CHARS)
    text = _BEARER_RE.sub("Bearer <redacted>", text)
    return _PATH_RE.sub("<path>", text)


def _normalise_error(value: object) -> dict[str, str] | None:
    if not isinstance(value, dict):
        return None
    code = value.get("code")
    if not isinstance(code, str) or code not in MEETING_ERROR_CODES:
        code = "unknown"
    message = _redact_error_message(value.get("message", ""))
    if not message:
        message = "meeting detector reported an error"
    return {"code": code, "message": message}


def _normalise_meeting(value: object) -> dict[str, Any] | None:
    if not isinstance(value, dict):
        return None
    platform = value.get("platform")
    if not isinstance(platform, str) or platform not in MEETING_PLATFORMS:
        platform = "unknown"

    duration = value.get("duration_s")
    if isinstance(duration, bool) or not isinstance(duration, (int, float)):
        duration = None
    elif duration < 0 or duration != duration or duration in (float("inf"), float("-inf")):
        duration = None

    started_at = _bounded_int(value.get("started_at_unix_ms"))
    attendee_count = _bounded_int(value.get("attendee_count")) or 0

    return {
        "id": _bounded_text(value.get("id", ""), MAX_ID_CHARS),
        "platform": platform,
        "active": value.get("active") is True,
        "started_at_unix_ms": started_at,
        "duration_s": duration,
        "title": _bounded_text(value.get("title", ""), MAX_TITLE_CHARS),
        "organizer": _bounded_text(value.get("organizer", ""), MAX_ORGANIZER_CHARS),
        "attendee_count": min(attendee_count, 1024),
        "has_calendar_metadata": value.get("has_calendar_metadata") is True,
    }


def _disabled_report(*, available: bool) -> dict[str, Any]:
    return {
        "available": available,
        "enabled": False,
        "state": "disabled",
        "meeting_state": "unknown",
        "connected": False,
        "manage_child": False,
        "detector_interval_secs": 0,
        "poll_interval_ms": 0,
        "restart_count": 0,
        "last_update_age_ms": None,
        "last_detection_unix_ms": None,
        "last_detection": None,
        "meetings": [],
        "error": None,
    }


def normalise_meeting_status(raw: object) -> dict[str, Any]:
    """Return a bounded, typed meeting report from a supervisor document.

    ``raw`` is the value of the supervisor's ``meeting`` key. ``None`` means the
    supervisor predates the report (or meeting detection is absent); the result
    is a disabled report, not an error, so older deployments keep working.
    """

    if raw is None:
        return _disabled_report(available=False)
    if not isinstance(raw, dict):
        report = _disabled_report(available=True)
        report["state"] = "unavailable"
        report["error"] = {
            "code": "protocol",
            "message": "supervisor meeting report is not an object",
        }
        return report

    state = raw.get("state")
    error = _normalise_error(raw.get("error"))
    if not isinstance(state, str) or state not in MEETING_DETECT_STATES:
        state = "unavailable"
        if error is None:
            error = {
                "code": "protocol",
                "message": "supervisor meeting state is not recognised",
            }

    meetings: list[dict[str, Any]] = []
    raw_meetings = raw.get("meetings")
    if isinstance(raw_meetings, list):
        for entry in raw_meetings[:MAX_MEETINGS]:
            meeting = _normalise_meeting(entry)
            if meeting is not None and meeting["active"]:
                meetings.append(meeting)

    meeting_state = raw.get("meeting_state")
    if not isinstance(meeting_state, str) or meeting_state not in MEETING_STATES:
        meeting_state = "unknown"

    # Never claim a call based only on the upstream summary. A real active
    # entry is required.
    if meeting_state == "active" and not meetings:
        meeting_state = "unknown"
        if error is None:
            error = {
                "code": "inconsistent_snapshot",
                "message": "supervisor reported an active call without a meeting entry",
            }
    elif meeting_state != "active" and meetings:
        meeting_state = "active"

    last_detection = None
    if meetings:
        last_detection = meetings[0]
    else:
        candidate = _normalise_meeting(raw.get("last_detection"))
        if candidate is not None:
            last_detection = candidate

    enabled = raw.get("enabled") is True
    if not enabled and state != "disabled":
        state = "disabled"

    return {
        "available": True,
        "enabled": enabled,
        "state": state,
        "meeting_state": meeting_state,
        "connected": raw.get("connected") is True,
        "manage_child": raw.get("manage_child") is True,
        "detector_interval_secs": _bounded_int(raw.get("detector_interval_secs")) or 0,
        "poll_interval_ms": _bounded_int(raw.get("poll_interval_ms")) or 0,
        "restart_count": _bounded_int(raw.get("restart_count")) or 0,
        "last_update_age_ms": _bounded_int(raw.get("last_update_age_ms")),
        "last_detection_unix_ms": _bounded_int(raw.get("last_detection_unix_ms")),
        "last_detection": last_detection,
        "meetings": meetings,
        "error": error,
    }
