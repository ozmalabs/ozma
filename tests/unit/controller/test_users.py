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


class TestConcurrentAccess:
    """Tests for concurrent user creation - race condition prevention."""

    def test_concurrent_create_users_same_username(self, tmp_path):
        """Test that concurrent attempts to create the same username raise ValueError for all but one."""
        from users import UserManager
        import threading
        import time

        manager = UserManager(tmp_path / "users_concurrent.json")
        errors = []
        success_count = [0]

        def create_user_task(username, results):
            try:
                u = manager.create_user(username, username, password="test")
                results.append(("success", u.username))
            except ValueError as e:
                results.append(("valueerror", str(e)))
            except Exception as e:
                errors.append(str(e))

        # Launch 10 threads trying to create the same user
        threads = []
        results = []
        for i in range(10):
            t = threading.Thread(target=create_user_task, args=("alice", results))
            threads.append(t)

        # Start all threads at approximately the same time
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        # Check that no exceptions other than ValueError occurred
        assert len(errors) == 0, f"Unexpected errors: {errors}"

        # Count successes - exactly one should succeed
        successes = [r for r in results if r[0] == "success"]
        assert len(successes) == 1, f"Expected 1 success, got {len(successes)}: {successes}"

        # Count ValueError exceptions - the rest should fail with ValueError
        valueerrors = [r for r in results if r[0] == "valueerror"]
        assert len(valueerrors) == 9, f"Expected 9 ValueErrors, got {len(valueerrors)}"

        # Verify only one user exists
        assert manager.user_count == 1
        assert manager.get_by_username("alice") is not None

    def test_concurrent_create_users_different_usernames(self, tmp_path):
        """Test that concurrent attempts to create different usernames all succeed."""
        from users import UserManager
        import threading

        manager = UserManager(tmp_path / "users_concurrent2.json")
        errors = []
        success_count = [0]
        lock = threading.Lock()

        def create_user_task(username):
            try:
                u = manager.create_user(username, username, password="test")
                with lock:
                    success_count[0] += 1
            except Exception as e:
                with lock:
                    errors.append(f"{username}: {e}")

        # Launch 10 threads trying to create different users
        threads = []
        for i in range(10):
            username = f"user_{i}"
            t = threading.Thread(target=create_user_task, args=(username,))
            threads.append(t)

        # Start all threads at approximately the same time
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        # Check that no exceptions occurred
        assert len(errors) == 0, f"Unexpected errors: {errors}"
        assert success_count[0] == 10, f"Expected 10 successes, got {success_count[0]}"

        # Verify all users were created
        assert manager.user_count == 10
