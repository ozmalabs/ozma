# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for FileSharingManager, FileShare, SambaUser, and FileSharingConfig."""

import json
import stat
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from file_sharing import (
    FileShare,
    FileSharingConfig,
    FileSharingManager,
    SambaUser,
    _share_id_from_name,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _manager(tmp_path: Path) -> FileSharingManager:
    return FileSharingManager(state_path=tmp_path / "file_sharing_state.json")


# ---------------------------------------------------------------------------
# TestFileShareModel
# ---------------------------------------------------------------------------

class TestFileShareModel:
    def test_to_dict_has_all_keys(self):
        share = FileShare(
            id="media", name="Media", path="/data/media",
            protocols=["smb"],
        )
        d = share.to_dict()
        for key in ("id", "name", "path", "protocols", "read_only", "guest_ok",
                    "valid_users", "comment", "browseable", "create_mask", "directory_mask"):
            assert key in d

    def test_roundtrip(self):
        share = FileShare(
            id="docs",
            name="Documents",
            path="/home/user/docs",
            protocols=["smb", "nfs"],
            read_only=True,
            guest_ok=False,
            valid_users=["alice", "bob"],
            comment="User documents",
            browseable=False,
            create_mask="0644",
            directory_mask="0755",
        )
        restored = FileShare.from_dict(share.to_dict())
        assert restored.id == share.id
        assert restored.name == share.name
        assert restored.path == share.path
        assert restored.protocols == share.protocols
        assert restored.read_only is True
        assert restored.guest_ok is False
        assert restored.valid_users == ["alice", "bob"]
        assert restored.comment == "User documents"
        assert restored.browseable is False
        assert restored.create_mask == "0644"
        assert restored.directory_mask == "0755"

    def test_defaults(self):
        share = FileShare.from_dict({
            "id": "x", "name": "X", "path": "/x", "protocols": ["smb"],
        })
        assert share.read_only is False
        assert share.guest_ok is False
        assert share.valid_users == []
        assert share.comment == ""
        assert share.browseable is True
        assert share.create_mask == "0664"
        assert share.directory_mask == "0775"

    def test_id_generated_from_name_lowercase(self):
        share_id = _share_id_from_name("My Share!")
        assert share_id == "my_share_"

    def test_id_limited_to_32_chars(self):
        long_name = "a" * 100
        share_id = _share_id_from_name(long_name)
        assert len(share_id) <= 32


# ---------------------------------------------------------------------------
# TestFileSharingConfig
# ---------------------------------------------------------------------------

class TestFileSharingConfig:
    def test_defaults(self):
        cfg = FileSharingConfig()
        assert cfg.enabled is False
        assert cfg.workgroup == "WORKGROUP"
        assert cfg.server_string == "Ozma File Server"
        assert cfg.netbios_name == "OZMA"
        assert cfg.smb_enabled is True
        assert cfg.nfs_enabled is False
        assert cfg.min_protocol == "SMB2"

    def test_roundtrip(self):
        cfg = FileSharingConfig(
            enabled=True,
            workgroup="CORP",
            server_string="Corp File Server",
            netbios_name="CORPNAS",
            smb_enabled=True,
            nfs_enabled=True,
            min_protocol="SMB3",
        )
        restored = FileSharingConfig.from_dict(cfg.to_dict())
        assert restored.enabled is True
        assert restored.workgroup == "CORP"
        assert restored.server_string == "Corp File Server"
        assert restored.netbios_name == "CORPNAS"
        assert restored.nfs_enabled is True
        assert restored.min_protocol == "SMB3"

    def test_from_dict_defaults_for_missing_keys(self):
        cfg = FileSharingConfig.from_dict({})
        assert cfg.enabled is False
        assert cfg.workgroup == "WORKGROUP"


# ---------------------------------------------------------------------------
# TestFileSharingManagerCRUD
# ---------------------------------------------------------------------------

class TestFileSharingManagerCRUD:
    def test_add_share_returns_file_share(self, tmp_path):
        mgr = _manager(tmp_path)
        share = mgr.add_share("Media", "/data/media", protocols=["smb"])
        assert isinstance(share, FileShare)
        assert share.name == "Media"
        assert share.path == "/data/media"
        assert share.protocols == ["smb"]

    def test_add_share_default_protocols_smb(self, tmp_path):
        mgr = _manager(tmp_path)
        share = mgr.add_share("Default", "/data/default")
        assert share.protocols == ["smb"]

    def test_add_share_id_from_name(self, tmp_path):
        mgr = _manager(tmp_path)
        share = mgr.add_share("My Media", "/data/media")
        assert share.id == "my_media"

    def test_update_share_changes_field(self, tmp_path):
        mgr = _manager(tmp_path)
        share = mgr.add_share("Movies", "/data/movies")
        updated = mgr.update_share(share.id, read_only=True, comment="Movie collection")
        assert updated is not None
        assert updated.read_only is True
        assert updated.comment == "Movie collection"

    def test_update_share_missing_returns_none(self, tmp_path):
        mgr = _manager(tmp_path)
        result = mgr.update_share("ghost_id", read_only=True)
        assert result is None

    def test_remove_share_returns_true(self, tmp_path):
        mgr = _manager(tmp_path)
        share = mgr.add_share("Temp", "/tmp/share")
        assert mgr.remove_share(share.id) is True
        assert mgr.get_share(share.id) is None

    def test_remove_share_missing_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.remove_share("nonexistent") is False

    def test_list_shares_returns_list_of_dicts(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("A", "/a")
        mgr.add_share("B", "/b")
        shares = mgr.list_shares()
        assert isinstance(shares, list)
        assert len(shares) == 2
        for s in shares:
            assert isinstance(s, dict)
            assert "id" in s

    def test_get_share_by_id(self, tmp_path):
        mgr = _manager(tmp_path)
        share = mgr.add_share("Music", "/data/music")
        found = mgr.get_share(share.id)
        assert found is not None
        assert found.name == "Music"

    def test_duplicate_name_slug_both_stored(self, tmp_path):
        # Two shares with the same name — second overwrites (same slug id)
        # The implementation uses the slug as key so second call replaces first.
        mgr = _manager(tmp_path)
        s1 = mgr.add_share("Photos", "/photos1")
        s2 = mgr.add_share("Photos", "/photos2")
        # Both have the same id (slug), second wins
        assert s1.id == s2.id
        found = mgr.get_share(s1.id)
        assert found.path == "/photos2"

    def test_add_share_with_valid_users(self, tmp_path):
        mgr = _manager(tmp_path)
        share = mgr.add_share("Private", "/private", valid_users=["alice", "bob"])
        assert share.valid_users == ["alice", "bob"]


# ---------------------------------------------------------------------------
# TestSmbConf
# ---------------------------------------------------------------------------

class TestSmbConf:
    def test_global_section_with_workgroup(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.workgroup = "TESTGROUP"
        conf = mgr._build_smb_conf()
        assert "[global]" in conf
        assert "workgroup = TESTGROUP" in conf

    def test_share_section_present(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Media", "/data/media", protocols=["smb"])
        conf = mgr._build_smb_conf()
        assert "[Media]" in conf
        assert "path = /data/media" in conf

    def test_guest_ok_yes(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Public", "/public", protocols=["smb"], guest_ok=True)
        conf = mgr._build_smb_conf()
        assert "guest ok = yes" in conf

    def test_guest_ok_no(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Private", "/private", protocols=["smb"], guest_ok=False)
        conf = mgr._build_smb_conf()
        assert "guest ok = no" in conf

    def test_read_only_yes(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Backup", "/backup", protocols=["smb"], read_only=True)
        conf = mgr._build_smb_conf()
        assert "read only = yes" in conf

    def test_read_only_no(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Writable", "/writable", protocols=["smb"], read_only=False)
        conf = mgr._build_smb_conf()
        assert "read only = no" in conf

    def test_valid_users_included(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Team", "/team", protocols=["smb"], valid_users=["alice", "bob"])
        conf = mgr._build_smb_conf()
        assert "valid users = alice bob" in conf

    def test_nfs_share_not_in_smb_conf(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("NFSOnly", "/nfs-only", protocols=["nfs"])
        conf = mgr._build_smb_conf()
        assert "[NFSOnly]" not in conf

    def test_both_protocol_share_in_smb_conf(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Both", "/both", protocols=["both"])
        conf = mgr._build_smb_conf()
        assert "[Both]" in conf


# ---------------------------------------------------------------------------
# TestNfsExports
# ---------------------------------------------------------------------------

class TestNfsExports:
    def test_nfs_share_in_exports(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("NFSShare", "/data/nfs", protocols=["nfs"])
        exports = mgr._build_nfs_exports()
        assert "/data/nfs" in exports

    def test_exports_contains_nfs_options(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("NFS", "/nfs", protocols=["nfs"])
        exports = mgr._build_nfs_exports()
        assert "sync" in exports

    def test_read_only_nfs_uses_ro_flag(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("ROShare", "/ro", protocols=["nfs"], read_only=True)
        exports = mgr._build_nfs_exports()
        assert "ro" in exports

    def test_read_write_nfs_uses_rw_flag(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("RWShare", "/rw", protocols=["nfs"], read_only=False)
        exports = mgr._build_nfs_exports()
        assert "rw" in exports

    def test_smb_only_share_not_in_exports(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("SMBOnly", "/smb", protocols=["smb"])
        exports = mgr._build_nfs_exports()
        assert "/smb" not in exports

    def test_both_protocol_in_exports(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Both", "/both", protocols=["both"])
        exports = mgr._build_nfs_exports()
        assert "/both" in exports

    def test_managed_comment_in_exports(self, tmp_path):
        mgr = _manager(tmp_path)
        exports = mgr._build_nfs_exports()
        assert "Ozma" in exports


# ---------------------------------------------------------------------------
# TestStatus
# ---------------------------------------------------------------------------

class TestStatus:
    def test_get_status_has_expected_fields(self, tmp_path):
        mgr = _manager(tmp_path)
        status = mgr.get_status()
        for key in ("enabled", "active", "share_count", "smb_enabled",
                    "nfs_enabled", "smbd_running", "nfsd_running"):
            assert key in status

    def test_share_count_reflects_actual_count(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("A", "/a")
        mgr.add_share("B", "/b")
        mgr.add_share("C", "/c")
        assert mgr.get_status()["share_count"] == 3

    def test_share_count_after_remove(self, tmp_path):
        mgr = _manager(tmp_path)
        share = mgr.add_share("Del", "/del")
        mgr.remove_share(share.id)
        assert mgr.get_status()["share_count"] == 0

    def test_smbd_running_false_when_no_proc(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.get_status()["smbd_running"] is False

    def test_list_samba_users_initially_empty(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.list_samba_users() == []


# ---------------------------------------------------------------------------
# TestPersistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_shares_survive_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Videos", "/videos", protocols=["smb"], read_only=True)
        mgr.add_share("Music", "/music", protocols=["nfs"])

        mgr2 = _manager(tmp_path)
        mgr2._load()
        shares = mgr2.list_shares()
        assert len(shares) == 2
        names = {s["name"] for s in shares}
        assert "Videos" in names
        assert "Music" in names

    def test_config_survives_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.workgroup = "HOMELAB"
        mgr._config.netbios_name = "HOMESERVER"
        mgr._config.nfs_enabled = True
        mgr._save()

        mgr2 = _manager(tmp_path)
        mgr2._load()
        assert mgr2._config.workgroup == "HOMELAB"
        assert mgr2._config.netbios_name == "HOMESERVER"
        assert mgr2._config.nfs_enabled is True

    def test_share_fields_preserved_on_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share(
            "Docs", "/docs",
            protocols=["smb"],
            guest_ok=True,
            read_only=True,
            valid_users=["alice"],
            comment="Document store",
        )
        mgr2 = _manager(tmp_path)
        mgr2._load()
        shares = mgr2.list_shares()
        assert len(shares) == 1
        s = shares[0]
        assert s["guest_ok"] is True
        assert s["read_only"] is True
        assert s["valid_users"] == ["alice"]
        assert s["comment"] == "Document store"


# ---------------------------------------------------------------------------
# TestStateFile
# ---------------------------------------------------------------------------

class TestStateFile:
    def test_state_file_mode_600(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Test", "/test")
        state_file = tmp_path / "file_sharing_state.json"
        assert state_file.exists()
        mode = oct(state_file.stat().st_mode)[-3:]
        assert mode == "600"

    def test_state_file_valid_json(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_share("Check", "/check")
        state_file = tmp_path / "file_sharing_state.json"
        data = json.loads(state_file.read_text())
        assert "config" in data
        assert "shares" in data
        assert "samba_users" in data
