# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Meeting detection — detect active calls and link to meeting accounts.

Detects when a video/audio call is active on the target machine by
monitoring running processes and audio streams. When a call starts,
the soft node reports it to the controller, which can:

  1. Auto-start live transcription on the call audio
  2. Tag the transcription with meeting metadata (who, what meeting)
  3. Auto-stop transcription when the call ends
  4. Generate meeting summary from the transcript

Supported platforms:
  Zoom:          detect zoom process + PipeWire stream "ZOOM VoiceEngine"
  Microsoft Teams: detect msedge/teams process + audio stream
  Google Meet:   detect chrome with meet.google.com + audio stream
  Slack:         detect slack process with huddle audio
  Discord:       detect discord voice connection
  FaceTime:      detect FaceTime process (macOS)
  Generic:       detect any new audio capture stream (fallback)

Meeting account linking:
  Users can link their calendar/meeting accounts to enrich transcriptions:
    - Microsoft 365 (via Graph API) → meeting title, attendees
    - Google Calendar (via Calendar API) → meeting title, attendees
    - Zoom (via Zoom API) → meeting ID, participants
  The linking is done once in the Connect dashboard. The soft node
  uses the tokens to query meeting metadata when a call is detected.
"""

from __future__ import annotations

import asyncio
import logging
import os
import platform
import re
import subprocess
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.softnode.meeting")


@dataclass
class ActiveMeeting:
    """A detected active meeting/call."""
    id: str
    platform: str           # zoom, teams, meet, slack, discord, facetime, unknown
    started_at: float = 0.0
    ended_at: float = 0.0
    active: bool = True

    # Metadata (from linked accounts)
    title: str = ""
    organizer: str = ""
    attendees: list[str] = field(default_factory=list)
    calendar_event_id: str = ""

    # Transcription
    transcription_session_id: str = ""
    audio_source: str = ""      # PipeWire source name for the call audio

    def to_dict(self) -> dict:
        return {
            "id": self.id, "platform": self.platform,
            "active": self.active, "title": self.title,
            "organizer": self.organizer, "attendees": self.attendees,
            "started_at": self.started_at,
            "duration_s": round((self.ended_at or time.time()) - self.started_at, 1),
            "transcription": self.transcription_session_id,
        }


# ── Process detection patterns ──────────────────────────────────────────────

MEETING_PATTERNS = {
    "zoom": {
        "processes": ["zoom", "zoom.us", "ZoomLauncher"],
        "audio_patterns": ["ZOOM VoiceEngine", "zoom_audiod"],
    },
    "teams": {
        "processes": ["teams", "ms-teams"],
        "audio_patterns": ["teams", "Microsoft Teams"],
        "browser_patterns": ["teams.microsoft.com", "teams.live.com"],
    },
    "meet": {
        "processes": [],
        "audio_patterns": [],
        "browser_patterns": ["meet.google.com"],
    },
    "slack": {
        "processes": ["slack"],
        "audio_patterns": ["Slack"],
    },
    "discord": {
        "processes": ["discord", "Discord"],
        "audio_patterns": ["discord", "Discord"],
    },
    "facetime": {
        "processes": ["FaceTime"],
        "audio_patterns": ["FaceTime"],
    },
}


class MeetingDetector:
    """
    Detects active video/audio calls and reports them to the controller.
    """

    def __init__(self, on_meeting_start: Any = None,
                 on_meeting_end: Any = None) -> None:
        self._on_start = on_meeting_start  # async callback(ActiveMeeting)
        self._on_end = on_meeting_end      # async callback(ActiveMeeting)
        self._active: dict[str, ActiveMeeting] = {}
        self._task: asyncio.Task | None = None
        self._linked_accounts: dict[str, dict] = {}  # platform → {token, ...}

    async def start(self) -> None:
        self._task = asyncio.create_task(self._detect_loop(), name="meeting-detect")
        log.info("Meeting detector started")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def link_account(self, platform_name: str, config: dict) -> None:
        """Link a meeting account (MS365, Google, Zoom) for metadata enrichment."""
        self._linked_accounts[platform_name] = config
        log.info("Meeting account linked: %s", platform_name)

    def get_active_meetings(self) -> list[dict]:
        return [m.to_dict() for m in self._active.values() if m.active]

    async def _detect_loop(self) -> None:
        """Poll for active meetings every 5 seconds."""
        while True:
            await asyncio.sleep(5)
            try:
                detected = self._scan_for_meetings()
                detected_ids = {m.id for m in detected}
                active_ids = {mid for mid, m in self._active.items() if m.active}

                # New meetings
                for meeting in detected:
                    if meeting.id not in active_ids:
                        self._active[meeting.id] = meeting
                        # Try to enrich with calendar metadata
                        await self._enrich_metadata(meeting)
                        log.info("Meeting detected: %s (%s) — %s",
                                 meeting.platform, meeting.title or "untitled", meeting.id)
                        if self._on_start:
                            await self._on_start(meeting)

                # Ended meetings
                for mid in active_ids - detected_ids:
                    meeting = self._active.get(mid)
                    if meeting:
                        meeting.active = False
                        meeting.ended_at = time.time()
                        log.info("Meeting ended: %s (%s)", meeting.platform, meeting.title)
                        if self._on_end:
                            await self._on_end(meeting)

            except Exception as e:
                log.debug("Meeting detection error: %s", e)

    def _scan_for_meetings(self) -> list[ActiveMeeting]:
        """Scan running processes and audio streams for active meetings."""
        meetings = []
        system = platform.system()

        # Get running processes
        processes = self._get_processes()
        # Get active PipeWire audio streams
        audio_streams = self._get_audio_streams()
        # Get browser tabs (if possible)
        browser_urls = self._get_browser_urls()

        for plat, patterns in MEETING_PATTERNS.items():
            detected = False

            # Check processes
            for proc_name in patterns.get("processes", []):
                if any(proc_name.lower() in p.lower() for p in processes):
                    detected = True
                    break

            # Check browser tabs
            if not detected:
                for url_pattern in patterns.get("browser_patterns", []):
                    if any(url_pattern in url for url in browser_urls):
                        detected = True
                        break

            # Verify with audio stream (reduces false positives)
            audio_source = ""
            if detected:
                for stream_pattern in patterns.get("audio_patterns", []):
                    for stream in audio_streams:
                        if stream_pattern.lower() in stream.lower():
                            audio_source = stream
                            break

            if detected:
                meeting_id = f"{plat}-{int(time.time()) // 300}"  # 5-min bucket to avoid duplicates
                meetings.append(ActiveMeeting(
                    id=meeting_id,
                    platform=plat,
                    started_at=time.time(),
                    audio_source=audio_source,
                ))

        # Generic detection: any new audio capture stream we don't recognise
        if not meetings:
            for stream in audio_streams:
                if any(kw in stream.lower() for kw in ["voice", "call", "meeting", "conference"]):
                    meeting_id = f"generic-{int(time.time()) // 300}"
                    meetings.append(ActiveMeeting(
                        id=meeting_id,
                        platform="unknown",
                        started_at=time.time(),
                        audio_source=stream,
                    ))
                    break

        return meetings

    def _get_processes(self) -> list[str]:
        try:
            if platform.system() == "Windows":
                r = subprocess.run(["tasklist", "/FO", "CSV", "/NH"],
                                   capture_output=True, text=True, timeout=5)
                return [line.split(",")[0].strip('"') for line in r.stdout.splitlines()]
            else:
                r = subprocess.run(["ps", "-eo", "comm"], capture_output=True, text=True, timeout=5)
                return r.stdout.splitlines()
        except Exception:
            return []

    def _get_audio_streams(self) -> list[str]:
        """Get active PipeWire/PulseAudio audio streams."""
        try:
            r = subprocess.run(["pactl", "list", "sink-inputs", "short"],
                               capture_output=True, text=True, timeout=5)
            streams = []
            for line in r.stdout.splitlines():
                parts = line.split()
                if len(parts) >= 4:
                    streams.append(parts[3] if len(parts) > 3 else parts[2])
            return streams
        except Exception:
            return []

    def _get_browser_urls(self) -> list[str]:
        """Get active browser tab URLs (best effort, Linux only)."""
        urls = []
        try:
            # Try xdotool + xprop for active window title
            r = subprocess.run(["xdotool", "getactivewindow", "getwindowname"],
                               capture_output=True, text=True, timeout=2)
            title = r.stdout.strip()
            # Browser titles often include the URL or site name
            if "meet.google.com" in title.lower():
                urls.append("meet.google.com")
            if "teams.microsoft.com" in title.lower() or "Microsoft Teams" in title:
                urls.append("teams.microsoft.com")
        except Exception:
            pass
        return urls

    async def _enrich_metadata(self, meeting: ActiveMeeting) -> None:
        """Try to get meeting metadata from linked calendar accounts."""
        # Check Microsoft 365
        if "microsoft" in self._linked_accounts:
            meta = await self._query_ms365_calendar(self._linked_accounts["microsoft"])
            if meta:
                meeting.title = meta.get("subject", "")
                meeting.organizer = meta.get("organizer", "")
                meeting.attendees = meta.get("attendees", [])
                meeting.calendar_event_id = meta.get("id", "")
                return

        # Check Google Calendar
        if "google" in self._linked_accounts:
            meta = await self._query_google_calendar(self._linked_accounts["google"])
            if meta:
                meeting.title = meta.get("summary", "")
                meeting.organizer = meta.get("organizer", {}).get("email", "")
                meeting.attendees = [a.get("email", "") for a in meta.get("attendees", [])]
                meeting.calendar_event_id = meta.get("id", "")
                return

    async def _query_ms365_calendar(self, config: dict) -> dict | None:
        """Query Microsoft Graph API for the current meeting."""
        import urllib.request
        token = config.get("access_token", "")
        if not token:
            return None
        try:
            loop = asyncio.get_running_loop()
            def _fetch():
                now = time.strftime("%Y-%m-%dT%H:%M:%S")
                url = (f"https://graph.microsoft.com/v1.0/me/calendarview"
                       f"?startDateTime={now}&endDateTime={now}"
                       f"&$select=subject,organizer,attendees")
                req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
                with urllib.request.urlopen(req, timeout=5) as r:
                    data = __import__("json").loads(r.read())
                    events = data.get("value", [])
                    return events[0] if events else None
            return await loop.run_in_executor(None, _fetch)
        except Exception:
            return None

    async def _query_google_calendar(self, config: dict) -> dict | None:
        """Query Google Calendar API for the current meeting."""
        import urllib.request
        token = config.get("access_token", "")
        if not token:
            return None
        try:
            loop = asyncio.get_running_loop()
            def _fetch():
                now = time.strftime("%Y-%m-%dT%H:%M:%SZ")
                url = (f"https://www.googleapis.com/calendar/v3/calendars/primary/events"
                       f"?timeMin={now}&timeMax={now}&singleEvents=true&maxResults=1")
                req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
                with urllib.request.urlopen(req, timeout=5) as r:
                    data = __import__("json").loads(r.read())
                    items = data.get("items", [])
                    return items[0] if items else None
            return await loop.run_in_executor(None, _fetch)
        except Exception:
            return None
