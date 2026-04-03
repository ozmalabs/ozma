# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for sharing.py — grant creation, expiry, peer management."""
import sys
import time
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


@pytest.fixture
def sharing_mgr(tmp_path):
    from sharing import SharingManager
    return SharingManager(tmp_path / "shares.json")


class TestShareGrants:
    def test_create_grant(self, sharing_mgr):
        g = sharing_mgr.create_grant(
            grantor_user_id="u-alice",
            grantee_user_id="u-bob",
            resource_type="service",
            resource_id="svc-jellyfin",
        )
        assert g.active
        assert not g.revoked
        assert g.grantor_user_id == "u-alice"
        assert g.grantee_user_id == "u-bob"

    def test_get_grant(self, sharing_mgr):
        g = sharing_mgr.create_grant("u-a", "u-b", "service", "svc-1")
        fetched = sharing_mgr.get_grant(g.id)
        assert fetched is not None
        assert fetched.id == g.id

    def test_revoke_grant(self, sharing_mgr):
        g = sharing_mgr.create_grant("u-a", "u-b", "service", "svc-1")
        assert sharing_mgr.revoke_grant(g.id)
        revoked = sharing_mgr.get_grant(g.id)
        assert revoked is not None
        assert revoked.revoked
        assert not revoked.active

    def test_revoke_nonexistent_returns_false(self, sharing_mgr):
        assert not sharing_mgr.revoke_grant("no-such-id")

    def test_list_grants_for_user(self, sharing_mgr):
        sharing_mgr.create_grant("u-alice", "u-bob", "service", "s1")
        sharing_mgr.create_grant("u-alice", "u-bob", "service", "s2")
        sharing_mgr.create_grant("u-carol", "u-dave", "service", "s3")
        bobs = sharing_mgr.list_grants_for_user("u-bob")
        assert len(bobs) == 2

    def test_list_grants_from_user(self, sharing_mgr):
        sharing_mgr.create_grant("u-alice", "u-bob", "service", "s1")
        sharing_mgr.create_grant("u-alice", "u-carol", "service", "s2")
        alices = sharing_mgr.list_grants_from_user("u-alice")
        assert len(alices) == 2

    def test_revoked_grant_excluded_from_active_lists(self, sharing_mgr):
        g = sharing_mgr.create_grant("u-alice", "u-bob", "service", "s1")
        sharing_mgr.revoke_grant(g.id)
        assert not sharing_mgr.list_grants_for_user("u-bob")
        assert not sharing_mgr.list_grants_from_user("u-alice")

    def test_grant_with_permissions(self, sharing_mgr):
        g = sharing_mgr.create_grant(
            "u-a", "u-b", "service", "svc-1",
            permissions=["read", "download"],
        )
        assert "read" in g.permissions
        assert "download" in g.permissions

    def test_grant_with_alias(self, sharing_mgr):
        g = sharing_mgr.create_grant(
            "u-a", "u-b", "service", "svc-1", alias="myjellyfin",
        )
        found = sharing_mgr.find_grant_by_alias("myjellyfin", "u-b")
        assert found is not None
        assert found.id == g.id

    def test_grant_with_expiry(self):
        from sharing import ShareGrant
        g = ShareGrant(
            id="g-exp", grantor_user_id="u-a", grantee_user_id="u-b",
            resource_type="service", resource_id="svc-1",
            expires_at=time.time() - 1,   # already expired
        )
        assert g.expired
        assert not g.active

    def test_persistence(self, tmp_path):
        from sharing import SharingManager
        mgr1 = SharingManager(tmp_path / "shares.json")
        g = mgr1.create_grant("u-a", "u-b", "service", "svc-1")
        mgr2 = SharingManager(tmp_path / "shares.json")
        assert mgr2.get_grant(g.id) is not None


class TestPeers:
    def test_add_peer(self, sharing_mgr):
        p = sharing_mgr.add_peer(
            controller_id="ctrl-bob",
            owner_user_id="u-alice",
            name="Bob's Controller",
            host="192.168.1.5",
        )
        assert p.id == "ctrl-bob"
        assert sharing_mgr.get_peer("ctrl-bob") is not None

    def test_add_peer_with_port(self, sharing_mgr):
        p = sharing_mgr.add_peer("ctrl-x", "u-1", "X Ctrl", "10.0.0.5", port=7390)
        assert p.port == 7390

    def test_remove_peer(self, sharing_mgr):
        sharing_mgr.add_peer("ctrl-x", "u-1", "X", "10.0.0.5")
        assert sharing_mgr.remove_peer("ctrl-x")
        assert sharing_mgr.get_peer("ctrl-x") is None

    def test_remove_nonexistent_peer_returns_false(self, sharing_mgr):
        assert not sharing_mgr.remove_peer("no-such-ctrl")

    def test_list_peers(self, sharing_mgr):
        sharing_mgr.add_peer("ctrl-a", "u-1", "A", "10.0.0.1")
        sharing_mgr.add_peer("ctrl-b", "u-1", "B", "10.0.0.2")
        assert len(sharing_mgr.list_peers()) == 2
