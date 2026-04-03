# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for users.py — UserManager CRUD and authentication."""
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


@pytest.fixture
def user_mgr(tmp_path):
    from users import UserManager
    return UserManager(tmp_path / "users.json")


class TestUserCRUD:
    def test_create_user(self, user_mgr):
        u = user_mgr.create_user("alice", "Alice", password="secret")
        assert u.username == "alice"
        assert u.display_name == "Alice"
        assert u.role == "owner"
        assert u.password_hash != "secret"   # must be hashed

    def test_create_user_member_role(self, user_mgr):
        u = user_mgr.create_user("bob", "Bob", role="member")
        assert u.role == "member"

    def test_duplicate_username_raises(self, user_mgr):
        user_mgr.create_user("alice", "Alice")
        with pytest.raises(ValueError):
            user_mgr.create_user("alice", "Alice 2")

    def test_get_user_by_id(self, user_mgr):
        u = user_mgr.create_user("carol", "Carol")
        fetched = user_mgr.get_user(u.id)
        assert fetched is not None
        assert fetched.username == "carol"

    def test_get_by_username(self, user_mgr):
        user_mgr.create_user("dave", "Dave")
        found = user_mgr.get_by_username("dave")
        assert found is not None
        assert found.display_name == "Dave"

    def test_get_unknown_user_returns_none(self, user_mgr):
        assert user_mgr.get_user("nonexistent-id") is None
        assert user_mgr.get_by_username("ghost") is None

    def test_list_users(self, user_mgr):
        user_mgr.create_user("u1", "User 1")
        user_mgr.create_user("u2", "User 2")
        users = user_mgr.list_users()
        assert len(users) == 2

    def test_delete_user(self, user_mgr):
        u = user_mgr.create_user("eve", "Eve")
        assert user_mgr.delete_user(u.id)
        assert user_mgr.get_user(u.id) is None

    def test_delete_nonexistent_returns_false(self, user_mgr):
        assert not user_mgr.delete_user("no-such-id")

    def test_update_display_name(self, user_mgr):
        u = user_mgr.create_user("frank", "Frank")
        updated = user_mgr.update_user(u.id, display_name="Franklin")
        assert updated is not None
        assert updated.display_name == "Franklin"
        assert user_mgr.get_user(u.id).display_name == "Franklin"

    def test_update_role(self, user_mgr):
        u = user_mgr.create_user("grace", "Grace")
        user_mgr.update_user(u.id, role="member")
        assert user_mgr.get_user(u.id).role == "member"


class TestAuthentication:
    def test_authenticate_success(self, user_mgr):
        user_mgr.create_user("carol", "Carol", password="pass123")
        u = user_mgr.authenticate("carol", "pass123")
        assert u is not None
        assert u.username == "carol"

    def test_authenticate_wrong_password(self, user_mgr):
        user_mgr.create_user("dave", "Dave", password="correct")
        assert user_mgr.authenticate("dave", "wrong") is None

    def test_authenticate_unknown_user(self, user_mgr):
        assert user_mgr.authenticate("nobody", "pw") is None

    def test_authenticate_no_password_set(self, user_mgr):
        user_mgr.create_user("nopass", "No Password")   # password=""
        assert user_mgr.authenticate("nopass", "") is None


class TestPersistence:
    def test_users_survive_reload(self, tmp_path):
        from users import UserManager
        mgr1 = UserManager(tmp_path / "users.json")
        mgr1.create_user("frank", "Frank", password="pw")
        # New manager instance loading same file
        mgr2 = UserManager(tmp_path / "users.json")
        assert mgr2.get_by_username("frank") is not None

    def test_user_count(self, user_mgr):
        assert user_mgr.user_count == 0
        user_mgr.create_user("a", "A")
        assert user_mgr.user_count == 1
        user_mgr.create_user("b", "B")
        assert user_mgr.user_count == 2
