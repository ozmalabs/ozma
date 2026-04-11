# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
SSH bastion — terminal access to mesh nodes via SSH.

Users connect with `ssh <node-id>@controller -p 2222` and land on the
node's serial console, VGA OCR terminal, or agent shell.  Every session
is audit-logged (hashchained).

Replaces the need for SSHFortress / Teleport / similar bastion products.
Built on asyncssh, reuses existing serial_console + terminal_renderer +
audit_log infrastructure.

Architecture:

  ssh mynode@controller:2222
       │
       ▼
  ┌──────────────────────��──────┐
  │  SSHBastionServer           │
  │  (asyncssh, port 2222)      │
  │                             │
  │  Auth: password (Authentik) │
  │  ├─ resolve username → node │
  │  ├─ consent check (workstation) │
  │  ├─ audit_log.log_event()   │
  │  └─ backend:                │
  │     ├─ serial console (preferred) │
  │     ├─ VGA terminal renderer│
  │     └─ agent shell          │
  └─────────────────────────────┘

Config:
  OZMA_SSH_BASTION=1              Enable (default: off)
  OZMA_SSH_BASTION_PORT=2222      Listen port
"""

from __future__ import annotations

import asyncio
import logging
import os
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.ssh_bastion")

try:
    import asyncssh
    _HAS_ASYNCSSH = True
except ImportError:
    asyncssh = None  # type: ignore[assignment]
    _HAS_ASYNCSSH = False

DEFAULT_PORT = 2222
HOST_KEY_PATH = Path(__file__).parent / "ssh_host_ed25519_key"


@dataclass
class BastionConfig:
    enabled: bool = False
    port: int = DEFAULT_PORT
    host_key_path: str = ""   # empty → auto-generate + persist

    @classmethod
    def from_env(cls) -> BastionConfig:
        return cls(
            enabled=os.environ.get("OZMA_SSH_BASTION", "0").lower() in ("1", "true", "yes"),
            port=int(os.environ.get("OZMA_SSH_BASTION_PORT", str(DEFAULT_PORT))),
            host_key_path=os.environ.get("OZMA_SSH_BASTION_HOST_KEY", ""),
        )


class SSHBastionServer:
    """
    SSH server on the controller that proxies terminal sessions to mesh nodes.

    Username = node ID.  Backend selection:
      1. Serial console (if node has serial attached)
      2. VGA terminal renderer (if node has display capture)
      3. Agent shell (if desktop agent is running on target)
    """

    def __init__(
        self,
        config: BastionConfig,
        state: Any = None,
        audit: Any = None,
        auth_config: Any = None,
        user_manager: Any = None,
    ) -> None:
        self._config = config
        self._state = state
        self._audit = audit
        self._auth_config = auth_config
        self._user_manager = user_manager
        self._server: Any = None
        self._host_key_path: Path | None = None
        self._active_sessions: dict[str, _BastionSession] = {}

    async def start(self) -> None:
        if not _HAS_ASYNCSSH:
            log.warning("asyncssh not installed — SSH bastion disabled. "
                        "Install with: pip install asyncssh")
            return
        if not self._config.enabled:
            return

        self._host_key_path = await self._ensure_host_key()
        if not self._host_key_path:
            log.error("Cannot start SSH bastion: no host key")
            return

        self._server = await asyncssh.create_server(
            lambda: _BastionSSHServer(self),
            "", self._config.port,
            server_host_keys=[str(self._host_key_path)],
            process_factory=self._handle_session,
        )
        log.info("SSH bastion listening on port %d", self._config.port)

    async def stop(self) -> None:
        if self._server:
            self._server.close()
            await self._server.wait_closed()
            log.info("SSH bastion stopped")
        for sess in list(self._active_sessions.values()):
            sess.close()

    # ��─ Host key management ─────────────────────────────────────────────

    async def _ensure_host_key(self) -> Path | None:
        """Load or generate the SSH host key."""
        path = Path(self._config.host_key_path) if self._config.host_key_path else HOST_KEY_PATH
        if path.exists():
            log.debug("SSH host key: %s", path)
            return path
        try:
            key = asyncssh.generate_private_key("ssh-ed25519")
            path.write_bytes(key.export_private_key())
            path.chmod(0o600)
            log.info("Generated SSH host key: %s", path)
            return path
        except Exception as e:
            log.error("Failed to generate SSH host key: %s", e)
            return None

    # ── Authentication ──────────────────────────────────────────────────

    def authenticate(self, username: str, password: str) -> bool:
        """Verify credentials against controller auth."""
        if not self._user_manager:
            return False
        user = self._user_manager.get_by_username(username)
        if not user:
            return False
        return self._user_manager.verify_password(username, password)

    # ── Node resolution ─────────────────────────────────────────────────

    def resolve_node(self, node_id: str) -> dict | None:
        """Look up a node by ID. Returns node dict or None."""
        if not self._state:
            return None
        node = self._state.get_node(node_id)
        if node:
            return node.to_dict() if hasattr(node, "to_dict") else {"id": node_id}
        # Try fuzzy match (prefix)
        for nid in self._state.node_ids():
            if nid.startswith(node_id):
                n = self._state.get_node(nid)
                return n.to_dict() if hasattr(n, "to_dict") else {"id": nid}
        return None

    # ── Session handling ────────────────────────────────────────────────

    def _handle_session(self, process: asyncssh.SSHServerProcess) -> None:
        """Called when an authenticated user opens a session."""
        asyncio.ensure_future(self._run_session(process))

    async def _run_session(self, process: asyncssh.SSHServerProcess) -> None:
        """Main session loop — connect user to the target node's console."""
        conn = process.get_extra_info("connection")
        # The SSH username is the target node ID; the authenticating user
        # is stored separately by the auth handler.
        target_node_id = process.get_extra_info("username", "")
        peer = process.get_extra_info("peername", ("?", 0))
        auth_user = getattr(conn, "_ozma_auth_user", "unknown")

        session_id = f"ssh-{target_node_id}-{int(time.time())}"

        # Audit: session start
        if self._audit:
            self._audit.log_event("ssh_session_start", "controller", {
                "session_id": session_id,
                "target_node": target_node_id,
                "auth_user": auth_user,
                "peer": f"{peer[0]}:{peer[1]}",
            })

        # Resolve target node
        node = self.resolve_node(target_node_id)
        if not node:
            process.stdout.write(f"Error: node '{target_node_id}' not found.\r\n")
            process.stdout.write("Available nodes:\r\n")
            if self._state:
                for nid in sorted(self._state.node_ids()):
                    process.stdout.write(f"  {nid}\r\n")
            process.exit(1)
            return

        resolved_id = node.get("id", target_node_id)
        process.stdout.write(f"Connected to {resolved_id}\r\n")

        # Find backend: serial console → VGA terminal → agent shell
        backend = await self._select_backend(resolved_id)
        if not backend:
            process.stdout.write(f"Error: no console backend available for {resolved_id}\r\n")
            process.stdout.write("Node must have serial console, display capture, or agent.\r\n")
            process.exit(1)
            return

        process.stdout.write(f"[{backend['type']}] Press Ctrl-] to disconnect.\r\n\r\n")

        # Session tracking
        sess = _BastionSession(
            session_id=session_id,
            node_id=resolved_id,
            auth_user=auth_user,
            backend_type=backend["type"],
            process=process,
            started_at=time.time(),
        )
        self._active_sessions[session_id] = sess

        try:
            match backend["type"]:
                case "serial":
                    await self._run_serial_session(process, backend["console_id"])
                case "vga":
                    await self._run_vga_session(process, resolved_id)
                case "agent":
                    await self._run_agent_session(process, resolved_id)
        except asyncssh.BreakReceived:
            pass
        except (asyncssh.ConnectionLost, BrokenPipeError, ConnectionResetError):
            pass
        except Exception as e:
            log.warning("SSH session %s error: %s", session_id, e)
        finally:
            self._active_sessions.pop(session_id, None)
            duration = time.time() - sess.started_at
            if self._audit:
                self._audit.log_event("ssh_session_end", "controller", {
                    "session_id": session_id,
                    "target_node": resolved_id,
                    "auth_user": auth_user,
                    "duration_s": round(duration, 1),
                    "backend": backend["type"],
                })
            process.exit(0)

    async def _select_backend(self, node_id: str) -> dict | None:
        """Pick the best console backend for a node."""
        # 1. Serial console
        if self._state and hasattr(self._state, "serial_consoles"):
            mgr = self._state.serial_consoles
            for cid in (f"{node_id}-serial", node_id):
                console = mgr.get_console(cid) if mgr else None
                if console and console.connected:
                    return {"type": "serial", "console_id": console.id}

        # 2. VGA / display capture (node has a stream)
        if self._state:
            node_info = self._state.get_node(node_id)
            if node_info and (node_info.vnc_host or node_info.stream_port):
                return {"type": "vga", "node_id": node_id}

        # 3. Agent shell (desktop agent with shell access)
        # Future: check agent WebSocket connection for shell capability

        return None

    # ── Serial console backend ──────────────────────────────────────────

    async def _run_serial_session(self, process: asyncssh.SSHServerProcess,
                                  console_id: str) -> None:
        """Bridge SSH ↔ serial console bidirectionally."""
        mgr = self._state.serial_consoles
        console = mgr.get_console(console_id)
        if not console:
            process.stdout.write("Serial console disconnected.\r\n")
            return

        # Dump recent scrollback
        recent = console.get_text(50)
        if recent:
            process.stdout.write(recent + "\r\n")

        # Bidirectional bridge: SSH stdin �� serial, serial output → SSH stdout
        read_task = asyncio.create_task(
            self._serial_to_ssh(process, console_id),
            name=f"ssh-serial-read-{console_id}",
        )
        write_task = asyncio.create_task(
            self._ssh_to_serial(process, console_id),
            name=f"ssh-serial-write-{console_id}",
        )
        done, pending = await asyncio.wait(
            [read_task, write_task],
            return_when=asyncio.FIRST_COMPLETED,
        )
        for t in pending:
            t.cancel()

    async def _serial_to_ssh(self, process: asyncssh.SSHServerProcess,
                             console_id: str) -> None:
        """Stream serial console output to SSH client."""
        mgr = self._state.serial_consoles
        last_len = len(mgr.get_console(console_id).buffer) if mgr.get_console(console_id) else 0
        while True:
            await asyncio.sleep(0.1)
            console = mgr.get_console(console_id)
            if not console or not console.connected:
                process.stdout.write("\r\n[serial disconnected]\r\n")
                return
            current_len = len(console.buffer)
            if current_len > last_len:
                new_lines = list(console.buffer)[last_len:]
                for line in new_lines:
                    process.stdout.write(line.text + "\r\n")
                last_len = current_len

    async def _ssh_to_serial(self, process: asyncssh.SSHServerProcess,
                             console_id: str) -> None:
        """Forward SSH client input to serial console."""
        mgr = self._state.serial_consoles
        while True:
            data = await process.stdin.read(1024)
            if not data:
                return
            # Ctrl-] = disconnect
            if b"\x1d" in data:
                return
            await mgr.send(console_id, data.decode("utf-8", errors="replace"))

    # ── VGA terminal backend ────────────────────────────────────────────

    async def _run_vga_session(self, process: asyncssh.SSHServerProcess,
                               node_id: str) -> None:
        """Render VGA display as ANSI text and stream to SSH client."""
        process.stdout.write("[VGA OCR mode — display renders as text]\r\n")
        # Import locally to avoid circular deps
        try:
            from terminal_renderer import render_frame_ansi
            from stream import StreamManager
        except ImportError:
            process.stdout.write("Terminal renderer not available.\r\n")
            return

        # Get terminal size
        width = process.get_extra_info("width", 80)
        height = process.get_extra_info("height", 24)

        while True:
            await asyncio.sleep(1.0)  # 1 FPS for text mode
            try:
                # Get current frame from the node's stream
                frame = await self._get_node_frame(node_id)
                if frame is None:
                    continue
                ansi = render_frame_ansi(frame, cols=width, rows=height - 1)
                # Move cursor to top-left, write frame
                process.stdout.write(f"\033[H{ansi}")
            except Exception:
                pass

    async def _get_node_frame(self, node_id: str) -> bytes | None:
        """Get the latest display frame for a node (JPEG bytes)."""
        # Try stream manager snapshot
        if self._state and hasattr(self._state, "streams"):
            streams = self._state.streams
            if hasattr(streams, "get_snapshot"):
                return await streams.get_snapshot(node_id)
        return None

    # ── Agent shell backend ─────────────────────────────────────────────

    async def _run_agent_session(self, process: asyncssh.SSHServerProcess,
                                 node_id: str) -> None:
        """Connect to the desktop agent's shell interface."""
        process.stdout.write("Agent shell not yet implemented.\r\n")
        process.stdout.write("Available backends: serial, vga\r\n")

    # ── Status ──────────────────────────────────────────────────────────

    def status(self) -> dict:
        return {
            "enabled": self._config.enabled,
            "port": self._config.port,
            "active_sessions": len(self._active_sessions),
            "sessions": [
                {
                    "id": s.session_id,
                    "node": s.node_id,
                    "user": s.auth_user,
                    "backend": s.backend_type,
                    "duration_s": round(time.time() - s.started_at, 1),
                }
                for s in self._active_sessions.values()
            ],
        }

    def list_sessions(self) -> list[dict]:
        return self.status()["sessions"]


# ── asyncssh server callbacks ──────────────────────────────────────────────

if _HAS_ASYNCSSH:

    class _BastionSSHServer(asyncssh.SSHServer):
        """Per-connection SSH server handler."""

        def __init__(self, bastion: SSHBastionServer) -> None:
            self._bastion = bastion
            self._conn: Any = None

        def connection_made(self, conn: asyncssh.SSHServerConnection) -> None:
            self._conn = conn
            peer = conn.get_extra_info("peername", ("?", 0))
            log.debug("SSH connection from %s:%d", peer[0], peer[1])

        def connection_lost(self, exc: Exception | None) -> None:
            pass

        def begin_auth(self, username: str) -> bool:
            """Return True to require authentication."""
            return True

        def password_auth_supported(self) -> bool:
            return True

        async def validate_password(self, username: str, password: str) -> bool:
            """Authenticate the SSH user.

            The SSH username field is the *target node ID*.  The password
            authenticates the *operator*.  We store the authenticated user
            on the connection object for audit logging.
            """
            if not self._bastion._user_manager:
                return False
            user = self._bastion._user_manager.authenticate_by_password(password)
            if user:
                self._conn._ozma_auth_user = user.get("username", "unknown")
                return True
            return False


@dataclass
class _BastionSession:
    """Tracks an active SSH session."""
    session_id: str
    node_id: str
    auth_user: str
    backend_type: str
    process: Any
    started_at: float

    def close(self) -> None:
        try:
            self.process.close()
        except Exception:
            pass
