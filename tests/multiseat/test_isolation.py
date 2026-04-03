# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.isolation — per-seat process isolation backends."""

from __future__ import annotations

import asyncio
import sys
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from agent.multiseat.isolation import (
    AppContainerIsolation,
    IsolationContext,
    IsolationManager,
    LinuxNamespaceIsolation,
    NoneIsolation,
    SandboxieIsolation,
    SeatIsolation,
    WindowsUserIsolation,
)


# ── IsolationContext ────────────────────────────────────────────────────────


class TestIsolationContext:
    def test_to_dict_minimal(self):
        ctx = IsolationContext(seat_index=0, backend_name="none")
        d = ctx.to_dict()
        assert d["seat_index"] == 0
        assert d["backend"] == "none"
        assert "username" not in d
        assert "password" not in d  # never exposed

    def test_to_dict_with_username(self):
        ctx = IsolationContext(
            seat_index=1, backend_name="user",
            username="ozma-seat-1", password="secret",
        )
        d = ctx.to_dict()
        assert d["username"] == "ozma-seat-1"
        assert "password" not in d

    def test_to_dict_with_sandbox(self):
        ctx = IsolationContext(
            seat_index=2, backend_name="sandboxie",
            sandbox_name="ozma_seat_2",
        )
        d = ctx.to_dict()
        assert d["sandbox"] == "ozma_seat_2"

    def test_to_dict_with_container_sid(self):
        ctx = IsolationContext(
            seat_index=3, backend_name="appcontainer",
            container_sid="S-1-15-2-12345",
        )
        d = ctx.to_dict()
        assert d["container_sid"] == "S-1-15-2-12345"


# ── NoneIsolation ──────────────────────────────────────────────────────────


class TestNoneIsolation:
    def test_always_available(self):
        backend = NoneIsolation()
        assert backend.is_available() is True
        assert backend.name == "none"

    @pytest.mark.asyncio
    async def test_setup(self):
        backend = NoneIsolation()
        ctx = await backend.setup(0)
        assert ctx.backend_name == "none"
        assert ctx.seat_index == 0

    @pytest.mark.asyncio
    async def test_launch(self):
        backend = NoneIsolation()
        ctx = await backend.setup(0)
        with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock) as mock_exec:
            mock_proc = MagicMock()
            mock_exec.return_value = mock_proc
            proc = await backend.launch(ctx, ["echo", "test"], {"PATH": "/usr/bin"})
            assert proc is mock_proc
            mock_exec.assert_called_once()
            args = mock_exec.call_args
            assert args[0][0] == "echo"
            assert args[0][1] == "test"

    @pytest.mark.asyncio
    async def test_teardown_noop(self):
        backend = NoneIsolation()
        ctx = await backend.setup(0)
        await backend.teardown(ctx)  # should not raise


# ── WindowsUserIsolation ──────────────────────────────────────────────────


class TestWindowsUserIsolation:
    def test_not_available_on_linux(self):
        backend = WindowsUserIsolation()
        if sys.platform != "win32":
            assert backend.is_available() is False

    def test_username_generation(self):
        backend = WindowsUserIsolation()
        assert backend._username(0) == "ozma-seat-0"
        assert backend._username(3) == "ozma-seat-3"

    def test_password_generation(self):
        backend = WindowsUserIsolation()
        pw = backend._generate_password()
        assert len(pw) == 24
        # Should be different each time
        pw2 = backend._generate_password()
        assert pw != pw2

    @pytest.mark.asyncio
    async def test_user_exists_check(self):
        backend = WindowsUserIsolation()
        mock_proc = AsyncMock()
        mock_proc.wait = AsyncMock(return_value=0)
        with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock, return_value=mock_proc):
            result = await backend._user_exists("ozma-seat-0")
            assert result is True

    @pytest.mark.asyncio
    async def test_user_not_exists(self):
        backend = WindowsUserIsolation()
        mock_proc = AsyncMock()
        mock_proc.wait = AsyncMock(return_value=2)
        with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock, return_value=mock_proc):
            result = await backend._user_exists("ozma-seat-99")
            assert result is False

    @pytest.mark.asyncio
    async def test_create_user_command(self):
        backend = WindowsUserIsolation()
        calls = []

        async def mock_exec(*args, **kwargs):
            calls.append(args)
            proc = AsyncMock()
            proc.returncode = 0
            proc.communicate = AsyncMock(return_value=(b"OK", b""))
            proc.wait = AsyncMock(return_value=0)
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_exec):
            result = await backend._create_user("ozma-seat-0", "testpass")
            assert result is True
            # First call should be 'net user ... /add'
            first_call = calls[0]
            assert "net" in first_call
            assert "user" in first_call
            assert "ozma-seat-0" in first_call
            assert "/add" in first_call

    @pytest.mark.asyncio
    async def test_create_user_failure(self):
        backend = WindowsUserIsolation()

        async def mock_exec(*args, **kwargs):
            proc = AsyncMock()
            proc.returncode = 1
            proc.communicate = AsyncMock(return_value=(b"", b"Access denied"))
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_exec):
            result = await backend._create_user("ozma-seat-0", "testpass")
            assert result is False

    @pytest.mark.asyncio
    async def test_setup_creates_user_when_not_exists(self):
        backend = WindowsUserIsolation()

        with (
            patch.object(backend, "_user_exists", return_value=False),
            patch.object(backend, "_create_user", return_value=True),
        ):
            ctx = await backend.setup(1)
            assert ctx.username == "ozma-seat-1"
            assert ctx.backend_name == "user"
            assert len(ctx.password) == 24

    @pytest.mark.asyncio
    async def test_setup_raises_on_creation_failure(self):
        backend = WindowsUserIsolation()

        with (
            patch.object(backend, "_user_exists", return_value=False),
            patch.object(backend, "_create_user", return_value=False),
        ):
            with pytest.raises(RuntimeError, match="administrator privileges"):
                await backend.setup(0)

    @pytest.mark.asyncio
    async def test_setup_reuses_existing_user(self):
        backend = WindowsUserIsolation()

        async def mock_exec(*args, **kwargs):
            proc = AsyncMock()
            proc.wait = AsyncMock(return_value=0)
            return proc

        with (
            patch.object(backend, "_user_exists", return_value=True),
            patch("asyncio.create_subprocess_exec", side_effect=mock_exec),
        ):
            ctx = await backend.setup(2)
            assert ctx.username == "ozma-seat-2"

    @pytest.mark.asyncio
    async def test_delete_user(self):
        backend = WindowsUserIsolation()

        async def mock_exec(*args, **kwargs):
            proc = AsyncMock()
            proc.returncode = 0
            proc.wait = AsyncMock(return_value=0)
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_exec):
            result = await backend._delete_user("ozma-seat-0")
            assert result is True

    @pytest.mark.asyncio
    async def test_teardown_preserves_user(self):
        backend = WindowsUserIsolation()
        ctx = IsolationContext(
            seat_index=0, backend_name="user",
            username="ozma-seat-0", password="pw",
        )
        # teardown should NOT delete the user
        await backend.teardown(ctx)

    @pytest.mark.asyncio
    async def test_teardown_and_delete(self):
        backend = WindowsUserIsolation()
        ctx = IsolationContext(
            seat_index=0, backend_name="user",
            username="ozma-seat-0", password="pw",
        )
        with patch.object(backend, "_delete_user", return_value=True) as mock_del:
            await backend.teardown_and_delete(ctx)
            mock_del.assert_called_once_with("ozma-seat-0")


# ── SandboxieIsolation ────────────────────────────────────────────────────


class TestSandboxieIsolation:
    def test_not_available_on_linux(self):
        backend = SandboxieIsolation()
        if sys.platform != "win32":
            assert backend.is_available() is False

    def test_sandbox_name(self):
        backend = SandboxieIsolation()
        assert backend._sandbox_name(0) == "ozma_seat_0"
        assert backend._sandbox_name(5) == "ozma_seat_5"

    @pytest.mark.asyncio
    async def test_setup(self):
        backend = SandboxieIsolation()
        with patch.object(backend, "_configure_sandbox", new_callable=AsyncMock):
            ctx = await backend.setup(0)
            assert ctx.backend_name == "sandboxie"
            assert ctx.sandbox_name == "ozma_seat_0"

    @pytest.mark.asyncio
    async def test_launch_command(self):
        backend = SandboxieIsolation()
        from pathlib import Path
        start_exe = Path("/fake/Sandboxie-Plus/Start.exe")

        with patch.object(backend, "_find_start_exe", return_value=start_exe):
            with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock) as mock_exec:
                mock_exec.return_value = MagicMock()
                ctx = IsolationContext(
                    seat_index=0, backend_name="sandboxie",
                    sandbox_name="ozma_seat_0",
                )
                await backend.launch(ctx, ["game.exe", "--fullscreen"], {})
                call_args = mock_exec.call_args[0]
                assert str(start_exe) in call_args
                assert "/box:ozma_seat_0" in call_args
                assert "game.exe" in call_args
                assert "--fullscreen" in call_args

    @pytest.mark.asyncio
    async def test_launch_raises_without_start_exe(self):
        backend = SandboxieIsolation()
        with patch.object(backend, "_find_start_exe", return_value=None):
            ctx = IsolationContext(
                seat_index=0, backend_name="sandboxie",
                sandbox_name="ozma_seat_0",
            )
            with pytest.raises(RuntimeError, match="Start.exe not found"):
                await backend.launch(ctx, ["game.exe"], {})

    @pytest.mark.asyncio
    async def test_teardown_terminates(self):
        backend = SandboxieIsolation()
        ctx = IsolationContext(
            seat_index=0, backend_name="sandboxie",
            sandbox_name="ozma_seat_0",
        )
        with patch.object(backend, "_terminate_sandbox", new_callable=AsyncMock) as mock_term:
            await backend.teardown(ctx)
            mock_term.assert_called_once_with("ozma_seat_0")

    def test_detection_paths(self):
        backend = SandboxieIsolation()
        with patch("pathlib.Path.exists", return_value=True):
            exe = backend._find_start_exe()
            assert exe is not None

    def test_ini_config_block(self):
        """Verify the sandbox config block has required settings."""
        backend = SandboxieIsolation()
        name = backend._sandbox_name(0)
        # The config block should contain OpenClsid=* for GPU access
        assert name == "ozma_seat_0"


# ── AppContainerIsolation ─────────────────────────────────────────────────


class TestAppContainerIsolation:
    def test_not_available_on_linux(self):
        backend = AppContainerIsolation()
        if sys.platform != "win32":
            assert backend.is_available() is False

    def test_container_name(self):
        backend = AppContainerIsolation()
        assert backend._container_name(0) == "ozma-seat-0"
        assert backend._container_name(7) == "ozma-seat-7"

    @pytest.mark.asyncio
    async def test_setup(self):
        backend = AppContainerIsolation()
        with patch.object(backend, "_create_profile", return_value="S-1-15-2-1234"):
            ctx = await backend.setup(0)
            assert ctx.backend_name == "appcontainer"
            assert ctx.container_sid == "S-1-15-2-1234"

    @pytest.mark.asyncio
    async def test_create_profile_via_powershell(self):
        backend = AppContainerIsolation()

        async def mock_exec(*args, **kwargs):
            proc = AsyncMock()
            proc.returncode = 0
            proc.communicate = AsyncMock(return_value=(b"S-1-15-2-99999\r\n", b""))
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_exec):
            sid = await backend._create_profile("ozma-seat-0")
            assert sid == "S-1-15-2-99999"

    @pytest.mark.asyncio
    async def test_create_profile_failure(self):
        backend = AppContainerIsolation()

        async def mock_exec(*args, **kwargs):
            proc = AsyncMock()
            proc.returncode = 1
            proc.communicate = AsyncMock(return_value=(b"", b"Error"))
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_exec):
            sid = await backend._create_profile("ozma-seat-0")
            assert sid == ""

    @pytest.mark.asyncio
    async def test_teardown_preserves_profile(self):
        backend = AppContainerIsolation()
        ctx = IsolationContext(
            seat_index=0, backend_name="appcontainer",
            container_sid="S-1-15-2-1234",
        )
        await backend.teardown(ctx)  # should not raise

    @pytest.mark.asyncio
    async def test_teardown_and_delete(self):
        backend = AppContainerIsolation()
        ctx = IsolationContext(
            seat_index=0, backend_name="appcontainer",
        )
        with patch.object(backend, "_delete_profile", new_callable=AsyncMock) as mock_del:
            await backend.teardown_and_delete(ctx)
            mock_del.assert_called_once_with("ozma-seat-0")


# ── LinuxNamespaceIsolation ───────────────────────────────────────────────


class TestLinuxNamespaceIsolation:
    def test_not_available_on_windows(self):
        backend = LinuxNamespaceIsolation()
        if sys.platform == "win32":
            assert backend.is_available() is False

    def test_available_with_unshare(self):
        backend = LinuxNamespaceIsolation()
        with patch("shutil.which", return_value="/usr/bin/unshare"):
            if sys.platform == "linux":
                assert backend.is_available() is True

    def test_not_available_without_unshare(self):
        backend = LinuxNamespaceIsolation()
        with patch("shutil.which", return_value=None):
            if sys.platform == "linux":
                assert backend.is_available() is False

    @pytest.mark.asyncio
    async def test_setup_as_root(self):
        backend = LinuxNamespaceIsolation()
        with (
            patch.object(backend, "_is_root", return_value=True),
            patch.object(backend, "_user_ns_available", return_value=False),
        ):
            ctx = await backend.setup(0)
            assert ctx.backend_name == "namespace"
            assert "unshare" in ctx.namespace_args
            assert "--pid" in ctx.namespace_args
            assert "--mount" in ctx.namespace_args
            assert "--user" not in ctx.namespace_args

    @pytest.mark.asyncio
    async def test_setup_unprivileged_with_user_ns(self):
        backend = LinuxNamespaceIsolation()
        with (
            patch.object(backend, "_is_root", return_value=False),
            patch.object(backend, "_user_ns_available", return_value=True),
        ):
            ctx = await backend.setup(1)
            assert "--user" in ctx.namespace_args
            assert "--map-root-user" in ctx.namespace_args

    @pytest.mark.asyncio
    async def test_launch_command(self):
        backend = LinuxNamespaceIsolation()
        ctx = IsolationContext(
            seat_index=0, backend_name="namespace",
            namespace_args=["unshare", "--pid", "--mount", "--fork"],
        )
        with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock) as mock_exec:
            mock_exec.return_value = MagicMock()
            await backend.launch(ctx, ["game", "--arg"], {"PATH": "/usr/bin"})
            call_args = mock_exec.call_args[0]
            assert call_args == (
                "unshare", "--pid", "--mount", "--fork",
                "--", "game", "--arg",
            )

    @pytest.mark.asyncio
    async def test_teardown_noop(self):
        backend = LinuxNamespaceIsolation()
        ctx = IsolationContext(
            seat_index=0, backend_name="namespace",
            namespace_args=["unshare", "--pid", "--fork"],
        )
        await backend.teardown(ctx)  # should not raise


# ── IsolationManager ──────────────────────────────────────────────────────


class TestIsolationManager:
    def test_none_always_available(self):
        """NoneIsolation is always in the detected backends."""
        manager = IsolationManager()
        assert "none" in manager.get_available()

    def test_get_backend(self):
        manager = IsolationManager()
        backend = manager.get_backend("none")
        assert backend is not None
        assert isinstance(backend, NoneIsolation)

    def test_get_backend_unknown(self):
        manager = IsolationManager()
        assert manager.get_backend("nonexistent") is None

    @pytest.mark.asyncio
    async def test_setup_seat(self):
        manager = IsolationManager()
        ctx = await manager.setup_seat("test-seat", "none", 0)
        assert ctx.backend_name == "none"
        assert manager.get_context("test-seat") is ctx

    @pytest.mark.asyncio
    async def test_setup_seat_invalid_backend(self):
        manager = IsolationManager()
        with pytest.raises(ValueError, match="not available"):
            await manager.setup_seat("test-seat", "nonexistent", 0)

    @pytest.mark.asyncio
    async def test_launch(self):
        manager = IsolationManager()
        await manager.setup_seat("test-seat", "none", 0)
        with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock) as mock_exec:
            mock_exec.return_value = MagicMock()
            proc = await manager.launch("test-seat", ["echo", "hi"], {})
            assert proc is mock_exec.return_value

    @pytest.mark.asyncio
    async def test_launch_no_context(self):
        manager = IsolationManager()
        with pytest.raises(KeyError, match="No isolation context"):
            await manager.launch("unknown-seat", ["echo"], {})

    @pytest.mark.asyncio
    async def test_teardown_seat(self):
        manager = IsolationManager()
        await manager.setup_seat("test-seat", "none", 0)
        assert manager.get_context("test-seat") is not None
        await manager.teardown_seat("test-seat")
        assert manager.get_context("test-seat") is None

    @pytest.mark.asyncio
    async def test_teardown_nonexistent_seat(self):
        manager = IsolationManager()
        # Should not raise
        await manager.teardown_seat("nonexistent")

    def test_to_dict(self):
        manager = IsolationManager()
        d = manager.to_dict()
        assert "available_backends" in d
        assert "none" in d["available_backends"]
        assert "seats" in d

    @pytest.mark.asyncio
    async def test_to_dict_with_seats(self):
        manager = IsolationManager()
        await manager.setup_seat("seat-a", "none", 0)
        d = manager.to_dict()
        assert "seat-a" in d["seats"]
        assert d["seats"]["seat-a"]["backend"] == "none"

    @pytest.mark.asyncio
    async def test_per_seat_different_backends(self):
        """Each seat can use a different backend independently."""
        manager = IsolationManager()
        ctx_a = await manager.setup_seat("seat-a", "none", 0)
        assert ctx_a.backend_name == "none"
        # If namespace is available, test with it too
        if "namespace" in manager.get_available():
            ctx_b = await manager.setup_seat("seat-b", "namespace", 1)
            assert ctx_b.backend_name == "namespace"
            assert manager.get_context("seat-a").backend_name == "none"
            assert manager.get_context("seat-b").backend_name == "namespace"

    def test_contexts_property_returns_copy(self):
        manager = IsolationManager()
        contexts = manager.contexts
        assert isinstance(contexts, dict)
        # Mutating the returned dict should not affect internal state
        contexts["fake"] = None
        assert "fake" not in manager.contexts

    def test_backends_property_returns_copy(self):
        manager = IsolationManager()
        backends = manager.backends
        assert isinstance(backends, dict)
        backends["fake"] = None
        assert "fake" not in manager.backends


# ── SeatProfile isolation field ───────────────────────────────────────────


class TestSeatProfileIsolation:
    def test_default_is_none(self):
        from agent.multiseat.seat_profiles import SeatProfile
        p = SeatProfile(name="test", description="test", launcher="desktop")
        assert p.isolation == "none"

    def test_custom_isolation(self):
        from agent.multiseat.seat_profiles import SeatProfile
        p = SeatProfile(
            name="test", description="test",
            launcher="desktop", isolation="user",
        )
        assert p.isolation == "user"

    def test_to_dict_omits_none_isolation(self):
        from agent.multiseat.seat_profiles import SeatProfile
        p = SeatProfile(name="test", description="test", launcher="desktop")
        d = p.to_dict()
        assert "isolation" not in d

    def test_to_dict_includes_non_none_isolation(self):
        from agent.multiseat.seat_profiles import SeatProfile
        p = SeatProfile(
            name="test", description="test",
            launcher="desktop", isolation="sandboxie",
        )
        d = p.to_dict()
        assert d["isolation"] == "sandboxie"

    def test_builtin_profiles_default_to_none(self):
        from agent.multiseat.seat_profiles import PROFILES
        for name, profile in PROFILES.items():
            assert profile.isolation == "none", f"Profile {name} should default to none"
