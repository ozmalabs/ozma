# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for ZFSManager, ZFSDataset, ZFSSnapshot, SnapshotPolicy."""

import asyncio
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from zfs_manager import (
    ZFSConfig,
    ZFSDataset,
    ZFSManagedDataset,
    ZFSManager,
    ZFSSnapshot,
    SnapshotPolicy,
    _parse_zfs_size,
    _parse_zfs_date,
    _snap_freq,
    _GMT_FMT,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _manager(tmp_path: Path, **kwargs) -> ZFSManager:
    return ZFSManager(state_path=tmp_path / "zfs_state.json", **kwargs)


def _snap(dataset: str, age_secs: float, short: str | None = None) -> ZFSSnapshot:
    created = time.time() - age_secs
    short = short or f"@GMT-2024.01.01-00.00.00"
    return ZFSSnapshot(
        name=f"{dataset}@{short.lstrip('@')}",
        dataset=dataset,
        short_name=short,
        created=created,
    )


# ---------------------------------------------------------------------------
# TestParseHelpers
# ---------------------------------------------------------------------------

class TestParseHelpers:
    def test_parse_size_k(self):
        assert _parse_zfs_size("1K") == 1024

    def test_parse_size_m(self):
        assert _parse_zfs_size("1M") == 1024 ** 2

    def test_parse_size_g(self):
        assert _parse_zfs_size("1.5G") == int(1.5 * 1024 ** 3)

    def test_parse_size_t(self):
        assert _parse_zfs_size("2T") == 2 * 1024 ** 4

    def test_parse_size_dash(self):
        assert _parse_zfs_size("-") == 0

    def test_parse_size_none_str(self):
        assert _parse_zfs_size("none") == 0

    def test_parse_size_integer(self):
        assert _parse_zfs_size("4096") == 4096

    def test_parse_date_standard(self):
        ts = _parse_zfs_date("Wed Jan 15 02:00 2024")
        assert ts > 0

    def test_parse_date_unknown_returns_zero(self):
        ts = _parse_zfs_date("not a date")
        assert ts == 0


# ---------------------------------------------------------------------------
# TestSnapFreq
# ---------------------------------------------------------------------------

class TestSnapFreq:
    def test_recent_is_hourly(self):
        s = _snap("tank/x", 3600)  # 1 hour old
        assert _snap_freq(s, [s]) == "hourly"

    def test_days_old_is_daily(self):
        s = _snap("tank/x", 86400 * 5)  # 5 days old
        assert _snap_freq(s, [s]) == "daily"

    def test_weeks_old_is_weekly(self):
        s = _snap("tank/x", 86400 * 20)  # 20 days old
        assert _snap_freq(s, [s]) == "weekly"

    def test_months_old_is_monthly(self):
        s = _snap("tank/x", 86400 * 90)  # 90 days old
        assert _snap_freq(s, [s]) == "monthly"


# ---------------------------------------------------------------------------
# TestSnapshotPolicy
# ---------------------------------------------------------------------------

class TestSnapshotPolicy:
    def test_defaults(self):
        p = SnapshotPolicy()
        assert p.hourly == 24
        assert p.daily == 30
        assert p.weekly == 52
        assert p.monthly == 12

    def test_roundtrip(self):
        p = SnapshotPolicy(hourly=4, daily=7, weekly=4, monthly=3)
        p2 = SnapshotPolicy.from_dict(p.to_dict())
        assert p2.hourly == 4
        assert p2.daily == 7
        assert p2.weekly == 4
        assert p2.monthly == 3

    def test_from_dict_defaults_for_missing(self):
        p = SnapshotPolicy.from_dict({})
        assert p.daily == 30


# ---------------------------------------------------------------------------
# TestZFSConfig
# ---------------------------------------------------------------------------

class TestZFSConfig:
    def test_defaults(self):
        cfg = ZFSConfig()
        assert cfg.enabled is False
        assert cfg.hourly_interval == 3600
        assert cfg.daily_interval == 86400
        assert cfg.default_encryption is True

    def test_roundtrip(self):
        cfg = ZFSConfig(enabled=True, hourly_interval=1800, default_encryption=False)
        cfg2 = ZFSConfig.from_dict(cfg.to_dict())
        assert cfg2.enabled is True
        assert cfg2.hourly_interval == 1800
        assert cfg2.default_encryption is False


# ---------------------------------------------------------------------------
# TestZFSManagedDataset
# ---------------------------------------------------------------------------

class TestZFSManagedDataset:
    def test_defaults(self):
        md = ZFSManagedDataset(dataset="tank/homes")
        assert md.auto_snapshot is True
        assert md.cloud_backup is False
        assert md.last_sent_snapshot is None

    def test_roundtrip(self):
        md = ZFSManagedDataset(
            dataset="tank/shares/docs",
            auto_snapshot=False,
            cloud_backup=True,
            last_sent_snapshot="@GMT-2024.01.15-02.00.00",
            last_sent_at=1700000000.0,
        )
        md2 = ZFSManagedDataset.from_dict(md.to_dict())
        assert md2.dataset == "tank/shares/docs"
        assert md2.auto_snapshot is False
        assert md2.cloud_backup is True
        assert md2.last_sent_snapshot == "@GMT-2024.01.15-02.00.00"
        assert md2.last_sent_at == 1700000000.0


# ---------------------------------------------------------------------------
# TestZFSManagerCRUD
# ---------------------------------------------------------------------------

class TestZFSManagerCRUD:
    def test_register_and_list(self, tmp_path):
        mgr = _manager(tmp_path)
        md = mgr.register_dataset("tank/homes")
        assert md.dataset == "tank/homes"
        lst = mgr.list_managed()
        assert len(lst) == 1
        assert lst[0]["dataset"] == "tank/homes"

    def test_register_twice_updates(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.register_dataset("tank/homes", auto_snapshot=True)
        md = mgr.register_dataset("tank/homes", auto_snapshot=False)
        assert md.auto_snapshot is False
        assert len(mgr.list_managed()) == 1

    def test_get_managed(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.register_dataset("tank/media")
        assert mgr.get_managed("tank/media") is not None
        assert mgr.get_managed("tank/other") is None

    def test_unregister(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.register_dataset("tank/docs")
        assert mgr.unregister_dataset("tank/docs") is True
        assert mgr.get_managed("tank/docs") is None

    def test_unregister_missing_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.unregister_dataset("tank/ghost") is False

    def test_update_managed(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.register_dataset("tank/homes")
        md = mgr.update_managed("tank/homes", cloud_backup=True)
        assert md is not None
        assert md.cloud_backup is True

    def test_update_managed_policy(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.register_dataset("tank/homes")
        md = mgr.update_managed("tank/homes", policy={"hourly": 4, "daily": 7})
        assert md.policy.hourly == 4
        assert md.policy.daily == 7

    def test_update_managed_missing(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.update_managed("tank/ghost", auto_snapshot=False) is None


# ---------------------------------------------------------------------------
# TestPersistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_managed_survives_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.register_dataset("tank/homes", cloud_backup=True)
        mgr.register_dataset("tank/media")

        mgr2 = _manager(tmp_path)
        lst = mgr2.list_managed()
        assert len(lst) == 2
        names = {d["dataset"] for d in lst}
        assert "tank/homes" in names
        assert "tank/media" in names

    def test_config_survives_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, hourly_interval=1800)

        mgr2 = _manager(tmp_path)
        assert mgr2.get_config().enabled is True
        assert mgr2.get_config().hourly_interval == 1800

    def test_state_file_mode_600(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.register_dataset("tank/test")
        state_file = tmp_path / "zfs_state.json"
        assert state_file.exists()
        mode = oct(state_file.stat().st_mode)[-3:]
        assert mode == "600"

    def test_timestamps_survive_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._last_hourly = 1700000000.0
        mgr._last_daily = 1700086400.0
        mgr._save()

        mgr2 = _manager(tmp_path)
        assert mgr2._last_hourly == 1700000000.0
        assert mgr2._last_daily == 1700086400.0


# ---------------------------------------------------------------------------
# TestSnapshotNaming
# ---------------------------------------------------------------------------

class TestSnapshotNaming:
    @pytest.mark.asyncio
    async def test_snapshot_uses_gmt_format(self, tmp_path):
        mgr = _manager(tmp_path)

        async def _fake_run_rc(*cmd):
            return 0

        with patch.object(mgr, "_run_rc", side_effect=_fake_run_rc):
            snap = await mgr.take_snapshot("tank/homes")

        assert snap is not None
        # ZFS snapshot: "dataset@GMT-..." (no double @@)
        assert "@GMT-" in snap
        assert "@@" not in snap

    @pytest.mark.asyncio
    async def test_snapshot_custom_label(self, tmp_path):
        mgr = _manager(tmp_path)

        async def _fake_run_rc(*cmd):
            return 0

        with patch.object(mgr, "_run_rc", side_effect=_fake_run_rc):
            snap = await mgr.take_snapshot("tank/homes", label="manual-backup")

        assert snap == "tank/homes@manual-backup"

    @pytest.mark.asyncio
    async def test_snapshot_returns_none_on_failure(self, tmp_path):
        mgr = _manager(tmp_path)

        async def _fake_run_rc(*cmd):
            return 1

        with patch.object(mgr, "_run_rc", side_effect=_fake_run_rc):
            snap = await mgr.take_snapshot("tank/homes")

        assert snap is None


# ---------------------------------------------------------------------------
# TestPruning
# ---------------------------------------------------------------------------

class TestPruning:
    @pytest.mark.asyncio
    async def test_prune_removes_excess_hourly(self, tmp_path):
        mgr = _manager(tmp_path)
        policy = SnapshotPolicy(hourly=2, daily=30, weekly=52, monthly=12)

        # 5 recent (hourly) snapshots
        snaps = [_snap("tank/x", age) for age in [100, 200, 300, 400, 500]]
        for idx, s in enumerate(snaps):
            s.short_name = f"@GMT-2024.01.01-0{idx}.00.00"

        destroyed_names = []

        async def _fake_list(dataset):
            return snaps

        async def _fake_destroy(name):
            destroyed_names.append(name)
            return True

        with patch.object(mgr, "list_snapshots", side_effect=_fake_list):
            with patch.object(mgr, "destroy_snapshot", side_effect=_fake_destroy):
                count = await mgr.prune_snapshots("tank/x", policy)

        # 5 hourly snapshots, keep 2 → destroy 3
        assert count == 3

    @pytest.mark.asyncio
    async def test_prune_skips_non_gmt_snapshots(self, tmp_path):
        mgr = _manager(tmp_path)
        policy = SnapshotPolicy(hourly=1, daily=1, weekly=1, monthly=1)

        # Mix of Ozma-managed GMT snaps and user-managed snaps
        snaps = [
            _snap("tank/x", 100, "@GMT-2024.01.01-00.00.00"),
            _snap("tank/x", 200, "@GMT-2024.01.01-01.00.00"),
            _snap("tank/x", 300, "@manual-backup"),    # NOT GMT-named
        ]

        destroyed_names = []

        async def _fake_list(dataset):
            return snaps

        async def _fake_destroy(name):
            destroyed_names.append(name)
            return True

        with patch.object(mgr, "list_snapshots", side_effect=_fake_list):
            with patch.object(mgr, "destroy_snapshot", side_effect=_fake_destroy):
                await mgr.prune_snapshots("tank/x", policy)

        # manual-backup should never be destroyed
        assert not any("manual-backup" in n for n in destroyed_names)


# ---------------------------------------------------------------------------
# TestLifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    @pytest.mark.asyncio
    async def test_start_creates_task_when_enabled(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.enabled = True
        mgr.register_dataset("tank/test")

        with patch.object(mgr, "_do_snapshot", AsyncMock()):
            await mgr.start()
            assert mgr._task is not None
            assert not mgr._task.done()
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_start_no_task_when_disabled(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.enabled = False
        await mgr.start()
        assert mgr._task is None

    @pytest.mark.asyncio
    async def test_stop_cancels_task(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.enabled = True
        mgr.register_dataset("tank/test")

        with patch.object(mgr, "_do_snapshot", AsyncMock()):
            await mgr.start()
            task = mgr._task
            await mgr.stop()

        assert task.done()
        assert mgr._task is None

    @pytest.mark.asyncio
    async def test_double_start_no_second_task(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.enabled = True
        mgr.register_dataset("tank/test")

        with patch.object(mgr, "_do_snapshot", AsyncMock()):
            await mgr.start()
            task1 = mgr._task
            await mgr.start()
            assert mgr._task is task1
            await mgr.stop()


# ---------------------------------------------------------------------------
# TestDiscovery (mocked zfs/zpool commands)
# ---------------------------------------------------------------------------

class TestDiscovery:
    @pytest.mark.asyncio
    async def test_list_pools_parses_output(self, tmp_path):
        mgr = _manager(tmp_path)
        output = "tank\tONLINE\t3.62T\t1.82T\t1.80T\n"

        with patch.object(mgr, "_run", AsyncMock(return_value=output)):
            pools = await mgr.list_pools()

        assert len(pools) == 1
        assert pools[0]["name"] == "tank"
        assert pools[0]["health"] == "ONLINE"

    @pytest.mark.asyncio
    async def test_list_pools_returns_empty_on_failure(self, tmp_path):
        mgr = _manager(tmp_path)

        with patch.object(mgr, "_run", AsyncMock(return_value=None)):
            pools = await mgr.list_pools()

        assert pools == []

    @pytest.mark.asyncio
    async def test_list_datasets_parses_output(self, tmp_path):
        mgr = _manager(tmp_path)
        output = "tank/homes\t/tank/homes\t50G\t200G\t50G\taes-256-gcm\tlz4\tnone\n"

        with patch.object(mgr, "_run", AsyncMock(return_value=output)):
            datasets = await mgr.list_datasets()

        assert len(datasets) == 1
        ds = datasets[0]
        assert ds.name == "tank/homes"
        assert ds.pool == "tank"
        assert ds.mountpoint == "/tank/homes"
        assert ds.encrypted is True
        assert ds.compression == "lz4"

    @pytest.mark.asyncio
    async def test_list_datasets_unencrypted(self, tmp_path):
        mgr = _manager(tmp_path)
        output = "tank/media\t/tank/media\t1T\t500G\t1T\toff\tlz4\tnone\n"

        with patch.object(mgr, "_run", AsyncMock(return_value=output)):
            datasets = await mgr.list_datasets()

        assert datasets[0].encrypted is False

    @pytest.mark.asyncio
    async def test_list_snapshots_parses_output(self, tmp_path):
        mgr = _manager(tmp_path)
        # ZFS snapshot name: dataset@snapname — snapname is "GMT-..." (no leading @)
        # "zfs list" output uses full name: "tank/homes@GMT-2024.01.15-02.00.00"
        output = (
            "tank/homes@GMT-2024.01.15-02.00.00\t"
            "Wed Jan 15 02:00 2024\t1.5G\n"
        )

        with patch.object(mgr, "_run", AsyncMock(return_value=output)):
            snaps = await mgr.list_snapshots("tank/homes")

        assert len(snaps) == 1
        s = snaps[0]
        assert s.dataset == "tank/homes"
        # short_name gets "@" prepended for API consistency
        assert s.short_name == "@GMT-2024.01.15-02.00.00"

    @pytest.mark.asyncio
    async def test_create_dataset_success(self, tmp_path):
        mgr = _manager(tmp_path)

        with patch.object(mgr, "_run_rc", AsyncMock(return_value=0)):
            ok = await mgr.create_dataset("tank/test", mountpoint="/tank/test",
                                           encrypted=False)
        assert ok is True

    @pytest.mark.asyncio
    async def test_create_dataset_failure(self, tmp_path):
        mgr = _manager(tmp_path)

        with patch.object(mgr, "_run_rc", AsyncMock(return_value=1)):
            ok = await mgr.create_dataset("tank/test")
        assert ok is False


# ---------------------------------------------------------------------------
# TestStatus
# ---------------------------------------------------------------------------

class TestStatus:
    def test_status_fields(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.register_dataset("tank/homes")
        mgr.register_dataset("tank/media")
        status = mgr.get_status()
        assert status["managed_datasets"] == 2
        assert status["enabled"] is False
        assert "scheduler_running" in status
        assert "datasets" in status
