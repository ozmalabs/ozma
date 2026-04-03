# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for V1.6 mobile camera additions:
  - ClipBrowser (clip browsing)
  - GuestTokenManager (camera-only guest tokens)
  - MotionPushManager (motion push webhooks)
"""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from mobile_camera import (
    ClipBrowser,
    ClipInfo,
    GuestCameraToken,
    GuestTokenManager,
    MotionPushManager,
    PushWebhook,
)


# ---------------------------------------------------------------------------
# ClipInfo dataclass
# ---------------------------------------------------------------------------

class TestClipInfo:
    def test_to_dict_keys(self):
        clip = ClipInfo(
            clip_id="clip001",
            camera_id="cam01",
            timestamp=1700000000.0,
            duration_sec=30.5,
            size_bytes=1_048_576,
            path="/recordings/cam01/clip001.mp4",
            thumbnail_url="/api/v1/cameras/cam01/clips/clip001/thumbnail",
            event_type="motion",
        )
        d = clip.to_dict()
        for k in ("clip_id", "camera_id", "timestamp", "duration_sec",
                  "size_bytes", "thumbnail_url", "event_type"):
            assert k in d

    def test_to_dict_no_path_exposed(self):
        clip = ClipInfo(
            clip_id="clip001", camera_id="cam01",
            timestamp=1700000000.0, duration_sec=30.0,
            size_bytes=1024, path="/secret/path/clip.mp4",
        )
        d = clip.to_dict()
        assert "path" not in d


# ---------------------------------------------------------------------------
# ClipBrowser
# ---------------------------------------------------------------------------

class TestClipBrowser:
    def _make_clip(self, cam_dir: Path, name: str, event_type: str = "") -> Path:
        """Create a dummy .mp4 clip file."""
        stem = f"{name}_{event_type}" if event_type else name
        p = cam_dir / f"{stem}.mp4"
        p.write_bytes(b"\x00" * 1024)
        return p

    def test_list_clips_returns_clips(self, tmp_path):
        cam_dir = tmp_path / "cam01"
        cam_dir.mkdir()
        self._make_clip(cam_dir, "clip_2024_01_15")
        browser = ClipBrowser(tmp_path)
        clips = browser.list_clips("cam01")
        assert len(clips) == 1
        assert clips[0].camera_id == "cam01"

    def test_list_clips_nonexistent_camera(self, tmp_path):
        browser = ClipBrowser(tmp_path)
        clips = browser.list_clips("nonexistent")
        assert clips == []

    def test_list_clips_limit(self, tmp_path):
        cam_dir = tmp_path / "cam01"
        cam_dir.mkdir()
        for i in range(10):
            self._make_clip(cam_dir, f"clip_{i:04d}")
        browser = ClipBrowser(tmp_path)
        clips = browser.list_clips("cam01", limit=5)
        assert len(clips) == 5

    def test_list_clips_event_type_filter(self, tmp_path):
        cam_dir = tmp_path / "cam01"
        cam_dir.mkdir()
        self._make_clip(cam_dir, "a", event_type="motion")
        self._make_clip(cam_dir, "b", event_type="object")
        browser = ClipBrowser(tmp_path)
        clips = browser.list_clips("cam01", event_type="motion")
        assert all(c.event_type == "motion" for c in clips)
        assert len(clips) == 1

    def test_list_clips_before_filter(self, tmp_path):
        cam_dir = tmp_path / "cam01"
        cam_dir.mkdir()
        p1 = self._make_clip(cam_dir, "old_clip")
        p2 = self._make_clip(cam_dir, "new_clip")
        # Set old mtime
        import os as _os
        old_ts = time.time() - 3600
        _os.utime(p1, (old_ts, old_ts))
        new_ts = time.time()
        _os.utime(p2, (new_ts, new_ts))

        browser = ClipBrowser(tmp_path)
        # Only clips with ts < (now - 1800)
        clips = browser.list_clips("cam01", before=time.time() - 1800)
        assert len(clips) == 1
        assert clips[0].clip_id == "old_clip"

    def test_get_clip_found(self, tmp_path):
        cam_dir = tmp_path / "cam01"
        cam_dir.mkdir()
        self._make_clip(cam_dir, "specific_clip")
        browser = ClipBrowser(tmp_path)
        clip = browser.get_clip("cam01", "specific_clip")
        assert clip is not None
        assert clip.clip_id == "specific_clip"

    def test_get_clip_not_found(self, tmp_path):
        cam_dir = tmp_path / "cam01"
        cam_dir.mkdir()
        browser = ClipBrowser(tmp_path)
        assert browser.get_clip("cam01", "nonexistent") is None

    def test_get_clip_unknown_camera(self, tmp_path):
        browser = ClipBrowser(tmp_path)
        assert browser.get_clip("nonexistent", "clip") is None

    def test_parse_event_type_motion(self, tmp_path):
        p = tmp_path / "clip001_motion.mp4"
        assert ClipBrowser._parse_event_type(p) == "motion"

    def test_parse_event_type_object(self, tmp_path):
        p = tmp_path / "clip001_object.mp4"
        assert ClipBrowser._parse_event_type(p) == "object"

    def test_parse_event_type_unknown(self, tmp_path):
        p = tmp_path / "clip001.mp4"
        assert ClipBrowser._parse_event_type(p) == ""

    def test_probe_duration_with_sidecar(self, tmp_path):
        p = tmp_path / "clip.mp4"
        sidecar = tmp_path / "clip.json"
        sidecar.write_text(json.dumps({"duration_sec": 45.5}))
        assert ClipBrowser._probe_duration(p) == 45.5

    def test_probe_duration_no_sidecar(self, tmp_path):
        p = tmp_path / "clip.mp4"
        assert ClipBrowser._probe_duration(p) == 0.0

    def test_thumbnail_url_with_jpg(self, tmp_path):
        p = tmp_path / "clip.mp4"
        thumb = tmp_path / "clip.jpg"
        thumb.write_bytes(b"\xff\xd8")  # JPEG magic
        url = ClipBrowser._thumbnail_url(p, "cam01")
        assert "cam01" in url
        assert "clip" in url

    def test_thumbnail_url_no_jpg(self, tmp_path):
        p = tmp_path / "clip.mp4"
        assert ClipBrowser._thumbnail_url(p, "cam01") == ""


# ---------------------------------------------------------------------------
# GuestCameraToken
# ---------------------------------------------------------------------------

class TestGuestCameraToken:
    def test_is_expired_future(self):
        gt = GuestCameraToken(
            token="tok", camera_ids=[], expires_at=time.time() + 86400
        )
        assert not gt.is_expired()

    def test_is_expired_past(self):
        gt = GuestCameraToken(
            token="tok", camera_ids=[], expires_at=time.time() - 1
        )
        assert gt.is_expired()

    def test_allows_camera_no_restriction(self):
        gt = GuestCameraToken(token="tok", camera_ids=[], expires_at=time.time() + 1)
        assert gt.allows_camera("any_cam")

    def test_allows_camera_restricted(self):
        gt = GuestCameraToken(
            token="tok", camera_ids=["cam01", "cam02"], expires_at=time.time() + 1
        )
        assert gt.allows_camera("cam01")
        assert not gt.allows_camera("cam03")

    def test_to_dict_keys(self):
        gt = GuestCameraToken(token="tok", camera_ids=["cam01"], expires_at=1700000000.0)
        d = gt.to_dict()
        assert "token" in d
        assert "camera_ids" in d
        assert "expires_at" in d


# ---------------------------------------------------------------------------
# GuestTokenManager
# ---------------------------------------------------------------------------

class TestGuestTokenManager:
    def test_create_token_returns_token(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        gt = mgr.create_token(label="Mum's phone")
        assert gt.token != ""
        assert not gt.is_expired()

    def test_create_token_with_camera_restriction(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        gt = mgr.create_token(camera_ids=["cam01"])
        assert gt.camera_ids == ["cam01"]

    def test_create_token_custom_ttl(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        gt = mgr.create_token(ttl_days=7)
        assert gt.expires_at <= time.time() + 7 * 86400 + 5

    def test_validate_valid_token(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        gt = mgr.create_token()
        assert mgr.validate(gt.token) is not None

    def test_validate_expired_token(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        gt = mgr.create_token(ttl_days=0)
        gt.expires_at = time.time() - 1  # forcibly expire
        assert mgr.validate(gt.token) is None

    def test_validate_unknown_token(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        assert mgr.validate("nonexistent") is None

    def test_revoke_token(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        gt = mgr.create_token()
        ok = mgr.revoke(gt.token)
        assert ok is True
        assert mgr.validate(gt.token) is None

    def test_revoke_unknown_token(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        assert mgr.revoke("nonexistent") is False

    def test_list_tokens(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        mgr.create_token(label="Token A")
        mgr.create_token(label="Token B")
        listing = mgr.list_tokens()
        assert len(listing) == 2

    def test_persistence(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        gt = mgr.create_token(label="Saved token")

        mgr2 = GuestTokenManager(data_dir=tmp_path)
        assert mgr2.validate(gt.token) is not None

    def test_save_file_permissions(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        mgr.create_token()
        p = tmp_path / "guest_tokens.json"
        assert p.exists()
        assert oct(p.stat().st_mode)[-3:] == "600"

    def test_load_missing_no_error(self, tmp_path):
        mgr = GuestTokenManager(data_dir=tmp_path)
        assert mgr.list_tokens() == []


# ---------------------------------------------------------------------------
# PushWebhook
# ---------------------------------------------------------------------------

class TestPushWebhook:
    def test_to_dict_keys(self):
        wh = PushWebhook(
            webhook_id="wh01",
            url="https://example.com/hook",
            camera_ids=["cam01"],
            events=["motion"],
        )
        d = wh.to_dict()
        for k in ("webhook_id", "url", "camera_ids", "events",
                  "label", "created_at", "last_fired_at", "failures"):
            assert k in d


# ---------------------------------------------------------------------------
# MotionPushManager
# ---------------------------------------------------------------------------

class TestMotionPushManager:
    def test_register_webhook(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = mgr.register(url="https://example.com/hook")
        assert wh.webhook_id != ""
        assert wh.url == "https://example.com/hook"

    def test_register_with_filters(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = mgr.register(
            url="https://example.com/hook",
            camera_ids=["cam01"],
            events=["motion"],
            label="My webhook",
        )
        assert wh.camera_ids == ["cam01"]
        assert wh.events == ["motion"]

    def test_unregister_webhook(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = mgr.register(url="https://example.com/hook")
        ok = mgr.unregister(wh.webhook_id)
        assert ok is True
        assert mgr.get_webhook(wh.webhook_id) is None

    def test_unregister_unknown(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        assert mgr.unregister("nonexistent") is False

    def test_list_webhooks(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        mgr.register(url="https://a.example.com/hook")
        mgr.register(url="https://b.example.com/hook")
        assert len(mgr.list_webhooks()) == 2

    @pytest.mark.asyncio
    async def test_notify_fires_matching_webhooks(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = mgr.register(url="https://example.com/hook")
        with patch.object(mgr, "_fire", AsyncMock(return_value=True)) as mock_fire:
            count = await mgr.notify("cam01", "motion")
        assert count == 1
        mock_fire.assert_called_once()

    @pytest.mark.asyncio
    async def test_notify_respects_camera_filter(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        mgr.register(url="https://example.com/hook", camera_ids=["cam02"])
        with patch.object(mgr, "_fire", AsyncMock(return_value=True)) as mock_fire:
            count = await mgr.notify("cam01", "motion")
        assert count == 0
        mock_fire.assert_not_called()

    @pytest.mark.asyncio
    async def test_notify_respects_event_filter(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        mgr.register(url="https://example.com/hook", events=["object"])
        with patch.object(mgr, "_fire", AsyncMock(return_value=True)) as mock_fire:
            count = await mgr.notify("cam01", "motion")  # motion, not object
        assert count == 0
        mock_fire.assert_not_called()

    @pytest.mark.asyncio
    async def test_notify_skips_over_failure_threshold(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = mgr.register(url="https://example.com/hook")
        wh.failures = MotionPushManager._MAX_FAILURES
        with patch.object(mgr, "_fire", AsyncMock(return_value=True)) as mock_fire:
            count = await mgr.notify("cam01", "motion")
        assert count == 0
        mock_fire.assert_not_called()

    @pytest.mark.asyncio
    async def test_notify_increments_failures_on_error(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        mgr.register(url="https://example.com/hook")
        with patch.object(mgr, "_fire", AsyncMock(return_value=False)):
            await mgr.notify("cam01", "motion")
        wh = list(mgr._webhooks.values())[0]
        assert wh.failures == 1

    @pytest.mark.asyncio
    async def test_notify_resets_failures_on_success(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = mgr.register(url="https://example.com/hook")
        wh.failures = 2
        with patch.object(mgr, "_fire", AsyncMock(return_value=True)):
            await mgr.notify("cam01", "motion")
        assert wh.failures == 0

    @pytest.mark.asyncio
    async def test_notify_updates_last_fired_at(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        mgr.register(url="https://example.com/hook")
        before = time.time()
        with patch.object(mgr, "_fire", AsyncMock(return_value=True)):
            await mgr.notify("cam01", "motion")
        wh = list(mgr._webhooks.values())[0]
        assert wh.last_fired_at >= before

    @pytest.mark.asyncio
    async def test_fire_success(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = PushWebhook(webhook_id="wh01", url="https://example.com/hook",
                         camera_ids=[], events=[])
        import urllib.request
        with patch("urllib.request.urlopen") as mock_urlopen:
            mock_urlopen.return_value = MagicMock()
            result = await mgr._fire(wh, b'{"event": "motion"}')
        assert result is True

    @pytest.mark.asyncio
    async def test_fire_failure(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = PushWebhook(webhook_id="wh01", url="https://example.com/hook",
                         camera_ids=[], events=[])
        with patch("urllib.request.urlopen", side_effect=Exception("timeout")):
            result = await mgr._fire(wh, b'{"event": "motion"}')
        assert result is False

    def test_persistence(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = mgr.register(url="https://example.com/hook", events=["motion"])

        mgr2 = MotionPushManager(data_dir=tmp_path)
        assert mgr2.get_webhook(wh.webhook_id) is not None
        assert mgr2.get_webhook(wh.webhook_id).url == "https://example.com/hook"

    def test_save_file_permissions(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        mgr.register(url="https://example.com/hook")
        p = tmp_path / "push_webhooks.json"
        assert p.exists()
        assert oct(p.stat().st_mode)[-3:] == "600"

    def test_load_missing_no_error(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        assert mgr.list_webhooks() == []

    def test_get_webhook_found(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        wh = mgr.register(url="https://example.com/hook")
        assert mgr.get_webhook(wh.webhook_id) is wh

    def test_get_webhook_not_found(self, tmp_path):
        mgr = MotionPushManager(data_dir=tmp_path)
        assert mgr.get_webhook("nonexistent") is None
