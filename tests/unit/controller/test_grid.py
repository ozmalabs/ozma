# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for controller/grid.py — V1.4 multi-Desk KVM federation.
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

from grid import DeskInfo, FeedSource, GridService, MarkClaim


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def make_desk(
    desk_id: str = "desk-a",
    name: str = "Desk A",
    host: str = "192.168.1.10",
    port: int = 7380,
    failover_group: str = "",
    priority: int = 0,
    online: bool = True,
) -> DeskInfo:
    desk = DeskInfo(id=desk_id, name=name, host=host, port=port,
                    failover_group=failover_group, priority=priority)
    desk.online = online
    return desk


def make_service(tmp_path: Path) -> GridService:
    return GridService(name="Test Grid", port=7381, data_dir=tmp_path / "grid")


# ---------------------------------------------------------------------------
# DeskInfo dataclass
# ---------------------------------------------------------------------------

class TestDeskInfo:
    def test_to_dict_roundtrip(self):
        desk = make_desk(desk_id="d1", failover_group="zone-a", priority=10)
        d = desk.to_dict()
        desk2 = DeskInfo.from_dict(d)
        assert desk2.id == "d1"
        assert desk2.failover_group == "zone-a"
        assert desk2.priority == 10

    def test_defaults(self):
        desk = DeskInfo(id="d1", name="D1", host="10.0.0.1", port=7380)
        assert desk.marks == []
        assert desk.failover_group == ""
        assert desk.priority == 0
        assert desk.online is True


# ---------------------------------------------------------------------------
# MarkClaim dataclass
# ---------------------------------------------------------------------------

class TestMarkClaim:
    def test_to_dict_roundtrip(self):
        claim = MarkClaim(mark_id="m1", desk_id="d1", shared=True)
        claim2 = MarkClaim.from_dict(claim.to_dict())
        assert claim2.mark_id == "m1"
        assert claim2.desk_id == "d1"
        assert claim2.shared is True

    def test_claimed_at_defaults_to_now(self):
        before = time.monotonic()
        claim = MarkClaim(mark_id="m1", desk_id="d1")
        assert claim.claimed_at >= before


# ---------------------------------------------------------------------------
# FeedSource dataclass
# ---------------------------------------------------------------------------

class TestFeedSource:
    def test_to_dict_roundtrip(self):
        feed = FeedSource(
            feed_id="f1", desk_id="d1", name="Feed 1",
            hls_url="http://10.0.0.1/feed.m3u8",
            rtsp_url="rtsp://10.0.0.1/live",
            audio=True,
            subscribers=["d2", "d3"],
        )
        d = feed.to_dict()
        feed2 = FeedSource.from_dict(d)
        assert feed2.feed_id == "f1"
        assert feed2.hls_url == "http://10.0.0.1/feed.m3u8"
        assert feed2.audio is True
        assert feed2.subscribers == ["d2", "d3"]

    def test_defaults(self):
        feed = FeedSource(feed_id="f1", desk_id="d1", name="F1")
        assert feed.hls_url == ""
        assert feed.rtsp_url == ""
        assert not feed.audio
        assert feed.subscribers == []
        assert feed.online is True


# ---------------------------------------------------------------------------
# Desk management
# ---------------------------------------------------------------------------

class TestDeskManagement:
    def test_register_desk(self, tmp_path):
        svc = make_service(tmp_path)
        desk = make_desk()
        svc.register_desk(desk)
        assert svc.get_desk("desk-a") is desk

    def test_list_desks_returns_dicts(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1"))
        svc.register_desk(make_desk("d2"))
        result = svc.list_desks()
        assert len(result) == 2
        assert all(isinstance(d, dict) for d in result)

    def test_unregister_desk(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1"))
        svc.unregister_desk("d1")
        assert svc.get_desk("d1") is None

    def test_unregister_releases_claims(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1"))
        svc.claim_mark("m1", "d1")
        svc.claim_mark("m2", "d1")
        svc.unregister_desk("d1")
        assert svc.get_claim("m1") is None
        assert svc.get_claim("m2") is None

    def test_unregister_marks_feeds_offline(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1"))
        svc.register_feed(FeedSource(feed_id="f1", desk_id="d1", name="F1"))
        svc.unregister_desk("d1")
        assert svc.get_feed("f1").online is False

    def test_update_desk(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1", priority=0))
        updated = svc.update_desk("d1", priority=10, failover_group="zone-a")
        assert updated.priority == 10
        assert updated.failover_group == "zone-a"

    def test_update_desk_not_found(self, tmp_path):
        svc = make_service(tmp_path)
        assert svc.update_desk("nonexistent", priority=5) is None

    def test_get_desk_not_found(self, tmp_path):
        svc = make_service(tmp_path)
        assert svc.get_desk("nonexistent") is None


# ---------------------------------------------------------------------------
# Mark claims
# ---------------------------------------------------------------------------

class TestMarkClaims:
    def test_claim_mark(self, tmp_path):
        svc = make_service(tmp_path)
        ok = svc.claim_mark("m1", "d1")
        assert ok is True
        claim = svc.get_claim("m1")
        assert claim.desk_id == "d1"

    def test_claim_same_desk_idempotent(self, tmp_path):
        svc = make_service(tmp_path)
        svc.claim_mark("m1", "d1")
        ok = svc.claim_mark("m1", "d1")
        assert ok is True
        assert svc.get_claim("m1").desk_id == "d1"

    def test_claim_transfers_to_new_desk(self, tmp_path):
        svc = make_service(tmp_path)
        svc.claim_mark("m1", "d1")
        svc.claim_mark("m1", "d2")
        assert svc.get_claim("m1").desk_id == "d2"

    def test_release_mark(self, tmp_path):
        svc = make_service(tmp_path)
        svc.claim_mark("m1", "d1")
        ok = svc.release_mark("m1", "d1")
        assert ok is True
        assert svc.get_claim("m1") is None

    def test_release_mark_wrong_desk(self, tmp_path):
        svc = make_service(tmp_path)
        svc.claim_mark("m1", "d1")
        ok = svc.release_mark("m1", "d2")  # d2 doesn't hold the claim
        assert ok is False
        assert svc.get_claim("m1") is not None

    def test_release_unclaimed_mark(self, tmp_path):
        svc = make_service(tmp_path)
        ok = svc.release_mark("nonexistent", "d1")
        assert ok is False

    def test_list_claims(self, tmp_path):
        svc = make_service(tmp_path)
        svc.claim_mark("m1", "d1")
        svc.claim_mark("m2", "d1")
        result = svc.list_claims()
        assert len(result) == 2
        mark_ids = {c["mark_id"] for c in result}
        assert mark_ids == {"m1", "m2"}

    def test_shared_claim(self, tmp_path):
        svc = make_service(tmp_path)
        svc.claim_mark("m1", "d1", shared=True)
        assert svc.get_claim("m1").shared is True


# ---------------------------------------------------------------------------
# Failover
# ---------------------------------------------------------------------------

class TestFailover:
    def test_failover_candidates_sorted_by_priority(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1", failover_group="zone-a", priority=0))
        svc.register_desk(make_desk("d2", failover_group="zone-a", priority=10))
        svc.register_desk(make_desk("d3", failover_group="zone-a", priority=5))
        candidates = svc.failover_candidates("d1")
        assert candidates[0].id == "d2"  # highest priority
        assert candidates[1].id == "d3"

    def test_failover_excludes_offline_desks(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1", failover_group="zone-a", priority=5))
        svc.register_desk(make_desk("d2", failover_group="zone-a", priority=10, online=False))
        candidates = svc.failover_candidates("d1")
        assert all(c.id != "d2" for c in candidates)

    def test_failover_excludes_different_group(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1", failover_group="zone-a"))
        svc.register_desk(make_desk("d2", failover_group="zone-b"))
        candidates = svc.failover_candidates("d1")
        assert candidates == []

    def test_failover_no_group_returns_empty(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1", failover_group=""))
        svc.register_desk(make_desk("d2", failover_group=""))
        assert svc.failover_candidates("d1") == []

    def test_do_failover_transfers_claims(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1", failover_group="zone-a", priority=0))
        svc.register_desk(make_desk("d2", failover_group="zone-a", priority=10))
        svc.claim_mark("m1", "d1")
        svc.claim_mark("m2", "d1")
        transferred = svc._do_failover("d1")
        assert set(transferred) == {"m1", "m2"}
        assert svc.get_claim("m1").desk_id == "d2"
        assert svc.get_claim("m2").desk_id == "d2"

    def test_do_failover_releases_when_no_candidates(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1", failover_group=""))
        svc.claim_mark("m1", "d1")
        svc._do_failover("d1")
        assert svc.get_claim("m1") is None

    def test_do_failover_only_transfers_failed_desks_claims(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1", failover_group="zone-a", priority=0))
        svc.register_desk(make_desk("d2", failover_group="zone-a", priority=10))
        svc.register_desk(make_desk("d3", failover_group="zone-a", priority=5))
        svc.claim_mark("m1", "d1")
        svc.claim_mark("m2", "d3")  # different desk — should not be touched
        svc._do_failover("d1")
        assert svc.get_claim("m2").desk_id == "d3"


# ---------------------------------------------------------------------------
# Feed sources
# ---------------------------------------------------------------------------

class TestFeedSources:
    def test_register_feed(self, tmp_path):
        svc = make_service(tmp_path)
        feed = FeedSource(feed_id="f1", desk_id="d1", name="Feed 1")
        svc.register_feed(feed)
        assert svc.get_feed("f1") is feed

    def test_unregister_feed(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_feed(FeedSource(feed_id="f1", desk_id="d1", name="F1"))
        ok = svc.unregister_feed("f1")
        assert ok is True
        assert svc.get_feed("f1") is None

    def test_unregister_unknown_feed(self, tmp_path):
        svc = make_service(tmp_path)
        assert svc.unregister_feed("nonexistent") is False

    def test_list_feeds(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_feed(FeedSource(feed_id="f1", desk_id="d1", name="F1"))
        svc.register_feed(FeedSource(feed_id="f2", desk_id="d2", name="F2"))
        result = svc.list_feeds()
        assert len(result) == 2

    def test_subscribe_feed(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_feed(FeedSource(feed_id="f1", desk_id="d1", name="F1"))
        ok = svc.subscribe_feed("f1", "d2")
        assert ok is True
        assert "d2" in svc.get_feed("f1").subscribers

    def test_subscribe_idempotent(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_feed(FeedSource(feed_id="f1", desk_id="d1", name="F1"))
        svc.subscribe_feed("f1", "d2")
        svc.subscribe_feed("f1", "d2")
        assert svc.get_feed("f1").subscribers.count("d2") == 1

    def test_subscribe_unknown_feed(self, tmp_path):
        svc = make_service(tmp_path)
        assert svc.subscribe_feed("nonexistent", "d2") is False

    def test_unsubscribe_feed(self, tmp_path):
        svc = make_service(tmp_path)
        feed = FeedSource(feed_id="f1", desk_id="d1", name="F1", subscribers=["d2"])
        svc.register_feed(feed)
        ok = svc.unsubscribe_feed("f1", "d2")
        assert ok is True
        assert "d2" not in svc.get_feed("f1").subscribers

    def test_unsubscribe_unknown_feed(self, tmp_path):
        svc = make_service(tmp_path)
        assert svc.unsubscribe_feed("nonexistent", "d2") is False


# ---------------------------------------------------------------------------
# Show state
# ---------------------------------------------------------------------------

class TestShowState:
    def test_show_state_structure(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1"))
        svc.claim_mark("m1", "d1")
        svc.register_feed(FeedSource(feed_id="f1", desk_id="d1", name="F1"))
        state = svc.show_state()
        assert "grid_name" in state
        assert "desks" in state
        assert "claims" in state
        assert "feeds" in state
        assert len(state["desks"]) == 1
        assert len(state["claims"]) == 1
        assert len(state["feeds"]) == 1


# ---------------------------------------------------------------------------
# Persistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_save_and_load_desks(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1", failover_group="zone-a", priority=5))
        svc._save()

        svc2 = make_service(tmp_path)
        assert "d1" in svc2._desks
        assert svc2._desks["d1"].failover_group == "zone-a"
        assert svc2._desks["d1"].priority == 5

    def test_save_and_load_claims(self, tmp_path):
        svc = make_service(tmp_path)
        svc.claim_mark("m1", "d1", shared=True)
        svc._save()

        svc2 = make_service(tmp_path)
        assert "m1" in svc2._claims
        assert svc2._claims["m1"].desk_id == "d1"
        assert svc2._claims["m1"].shared is True

    def test_save_and_load_feeds(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_feed(FeedSource(
            feed_id="f1", desk_id="d1", name="F1",
            hls_url="http://10.0.0.1/f1.m3u8", subscribers=["d2"],
        ))
        svc._save()

        svc2 = make_service(tmp_path)
        assert "f1" in svc2._feeds
        feed = svc2._feeds["f1"]
        assert feed.hls_url == "http://10.0.0.1/f1.m3u8"
        assert feed.subscribers == ["d2"]

    def test_save_file_permissions(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk())
        svc._save()
        p = tmp_path / "grid" / "grid_state.json"
        assert p.exists()
        assert oct(p.stat().st_mode)[-3:] == "600"

    def test_load_missing_no_error(self, tmp_path):
        svc = make_service(tmp_path)
        assert svc._desks == {}
        assert svc._claims == {}
        assert svc._feeds == {}

    def test_load_corrupt_no_error(self, tmp_path):
        grid_dir = tmp_path / "grid"
        grid_dir.mkdir()
        (grid_dir / "grid_state.json").write_text("{corrupt{{")
        svc = GridService(data_dir=grid_dir)
        assert svc._desks == {}

    def test_register_desk_saves(self, tmp_path):
        svc = make_service(tmp_path)
        svc.register_desk(make_desk("d1"))
        svc2 = make_service(tmp_path)
        assert "d1" in svc2._desks

    def test_claim_mark_saves(self, tmp_path):
        svc = make_service(tmp_path)
        svc.claim_mark("m1", "d1")
        svc2 = make_service(tmp_path)
        assert "m1" in svc2._claims


# ---------------------------------------------------------------------------
# Health loop
# ---------------------------------------------------------------------------

class TestHealthLoop:
    @pytest.mark.asyncio
    async def test_health_loop_marks_desk_offline(self, tmp_path):
        svc = make_service(tmp_path)
        desk = make_desk("d1", failover_group="zone-a")
        svc.register_desk(desk)

        with patch.object(svc, "_check_desk_health", AsyncMock(return_value=False)):
            # Run one iteration of the health check
            for d in list(svc._desks.values()):
                alive = await svc._check_desk_health(d)
                if not alive and d.online:
                    d.online = False
                    svc._do_failover(d.id)

        assert svc._desks["d1"].online is False

    @pytest.mark.asyncio
    async def test_health_loop_marks_desk_online_again(self, tmp_path):
        svc = make_service(tmp_path)
        desk = make_desk("d1")
        desk.online = False
        svc.register_desk(desk)

        with patch.object(svc, "_check_desk_health", AsyncMock(return_value=True)):
            for d in list(svc._desks.values()):
                alive = await svc._check_desk_health(d)
                if alive and not d.online:
                    d.online = True

        assert svc._desks["d1"].online is True


# ---------------------------------------------------------------------------
# Lifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    @pytest.mark.asyncio
    async def test_stop_cancels_tasks(self, tmp_path):
        svc = make_service(tmp_path)
        cancelled = []

        async def slow():
            try:
                await asyncio.sleep(9999)
            except asyncio.CancelledError:
                cancelled.append(True)
                raise

        task = asyncio.create_task(slow())
        svc._tasks.append(task)
        await asyncio.sleep(0)
        await svc.stop()
        assert cancelled

    @pytest.mark.asyncio
    async def test_start_creates_data_dir(self, tmp_path):
        data_dir = tmp_path / "nested" / "grid"
        svc = GridService(data_dir=data_dir)
        azc_mock = AsyncMock()
        azc_mock.async_register_service = AsyncMock()
        azc_mock.async_unregister_service = AsyncMock()
        azc_mock.async_close = AsyncMock()
        with patch.object(svc, "_health_loop", AsyncMock()), \
             patch("grid.AsyncZeroconf", return_value=azc_mock), \
             patch("grid.ServiceInfo"):
            await svc.start()
            await svc.stop()
        assert data_dir.exists()
