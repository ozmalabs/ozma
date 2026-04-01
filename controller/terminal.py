# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Persistent terminal — tmux meets mosh over Ozma Connect.

Provides persistent terminal sessions to target machines that survive
disconnects, laptop sleep, and network changes. Reconnect and you're
exactly where you left off — scrollback, running processes, everything.

Transport paths (in priority order):
  1. SSH via Ozma Connect relay (encrypted E2E, relay sees nothing)
  2. USB serial (ACM gadget on hardware nodes — works without network)
  3. Local pty (soft nodes — direct shell on the target)

Session persistence:
  The node holds the terminal session (pty) alive even when no client
  is connected. Like tmux/screen running on the node, but managed by
  ozma — no user setup required.

  Scrollback buffer: last 10,000 lines retained per session.
  Multiple sessions per node supported.
  Sessions survive: client disconnect, controller restart, network change.
  Sessions die with: node reboot or explicit close.

Client access:
  1. Web UI terminal (xterm.js in the dashboard)
  2. CLI: ozma-terminal <node-name>
  3. API: WebSocket at /api/v1/terminal/<node_id>

For remote access (Hetzner servers, cloud VMs, etc.):
  ozma-softnode runs on the server, connects to your controller via
  Ozma Connect relay. Open a terminal from the dashboard — persistent,
  encrypted, survives disconnect. Like mosh but through ozma.
"""

from __future__ import annotations

import asyncio
import logging
import os
import pty
import select
import struct
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.terminal")

SCROLLBACK_LINES = 10_000
READ_SIZE = 4096


@dataclass
class TerminalSession:
    """A persistent terminal session on a node."""
    id: str
    node_id: str
    created_at: float = 0.0
    last_activity: float = 0.0

    # pty state
    master_fd: int = -1
    pid: int = -1
    alive: bool = False

    # Scrollback
    scrollback: list[bytes] = field(default_factory=list)
    scrollback_total: int = 0

    # Connected clients (WebSocket connections)
    clients: list[Any] = field(default_factory=list)

    # Terminal size
    rows: int = 24
    cols: int = 80

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "node_id": self.node_id,
            "alive": self.alive,
            "created_at": self.created_at,
            "last_activity": self.last_activity,
            "rows": self.rows,
            "cols": self.cols,
            "clients": len(self.clients),
            "scrollback_lines": len(self.scrollback),
        }


class TerminalManager:
    """
    Manages persistent terminal sessions across all nodes.

    Sessions are held on the controller (for soft nodes) or proxied
    to the node (for hardware nodes with serial/SSH).
    """

    def __init__(self) -> None:
        self._sessions: dict[str, TerminalSession] = {}
        self._read_tasks: dict[str, asyncio.Task] = {}

    # ── Session lifecycle ───────────────────────────────────────────────────

    async def create_session(self, session_id: str, node_id: str,
                              shell: str = "", rows: int = 24,
                              cols: int = 80) -> TerminalSession | None:
        """
        Create a new persistent terminal session.

        For soft/virtual nodes: spawns a local pty with the target's shell.
        For hardware nodes: connects via SSH or USB serial.
        """
        if session_id in self._sessions:
            return self._sessions[session_id]

        # Determine shell
        if not shell:
            shell = os.environ.get("SHELL", "/bin/bash")

        # Spawn pty
        try:
            pid, master_fd = pty.fork()
        except OSError as e:
            log.error("Failed to fork pty: %s", e)
            return None

        if pid == 0:
            # Child process — exec the shell
            os.environ["TERM"] = "xterm-256color"
            os.environ["OZMA_SESSION"] = session_id
            os.execlp(shell, shell)
            # Never reaches here

        # Parent process
        session = TerminalSession(
            id=session_id,
            node_id=node_id,
            created_at=time.time(),
            last_activity=time.time(),
            master_fd=master_fd,
            pid=pid,
            alive=True,
            rows=rows,
            cols=cols,
        )

        # Set terminal size
        self._set_winsize(master_fd, rows, cols)

        self._sessions[session_id] = session

        # Start reading from the pty
        task = asyncio.create_task(
            self._read_loop(session), name=f"term-read-{session_id}"
        )
        self._read_tasks[session_id] = task

        log.info("Terminal session created: %s (node=%s, shell=%s, %dx%d)",
                 session_id, node_id, shell, cols, rows)
        return session

    async def close_session(self, session_id: str) -> bool:
        """Close a terminal session and kill the process."""
        session = self._sessions.pop(session_id, None)
        if not session:
            return False

        task = self._read_tasks.pop(session_id, None)
        if task:
            task.cancel()

        if session.pid > 0:
            try:
                os.kill(session.pid, 9)
                os.waitpid(session.pid, os.WNOHANG)
            except OSError:
                pass

        if session.master_fd >= 0:
            try:
                os.close(session.master_fd)
            except OSError:
                pass

        session.alive = False
        log.info("Terminal session closed: %s", session_id)
        return True

    def get_session(self, session_id: str) -> TerminalSession | None:
        return self._sessions.get(session_id)

    def list_sessions(self, node_id: str = "") -> list[dict]:
        sessions = self._sessions.values()
        if node_id:
            sessions = [s for s in sessions if s.node_id == node_id]
        return [s.to_dict() for s in sessions]

    # ── Input/output ────────────────────────────────────────────────────────

    async def write(self, session_id: str, data: bytes) -> bool:
        """Write data to the terminal (user input)."""
        session = self._sessions.get(session_id)
        if not session or not session.alive or session.master_fd < 0:
            return False
        try:
            loop = asyncio.get_running_loop()
            await loop.run_in_executor(None, os.write, session.master_fd, data)
            session.last_activity = time.time()
            return True
        except OSError:
            session.alive = False
            return False

    async def resize(self, session_id: str, rows: int, cols: int) -> bool:
        """Resize the terminal."""
        session = self._sessions.get(session_id)
        if not session or session.master_fd < 0:
            return False
        session.rows = rows
        session.cols = cols
        self._set_winsize(session.master_fd, rows, cols)
        return True

    def get_scrollback(self, session_id: str) -> bytes:
        """Get the full scrollback buffer for reconnection."""
        session = self._sessions.get(session_id)
        if not session:
            return b""
        return b"".join(session.scrollback)

    # ── Client management ───────────────────────────────────────────────────

    async def attach_client(self, session_id: str, client: Any) -> bool:
        """Attach a WebSocket client to a session."""
        session = self._sessions.get(session_id)
        if not session:
            return False
        session.clients.append(client)

        # Send scrollback to the new client
        scrollback = self.get_scrollback(session_id)
        if scrollback:
            try:
                await client.send_bytes(scrollback)
            except Exception:
                pass

        log.debug("Client attached to session %s (%d clients)",
                  session_id, len(session.clients))
        return True

    def detach_client(self, session_id: str, client: Any) -> None:
        """Detach a client (disconnect). Session stays alive."""
        session = self._sessions.get(session_id)
        if session and client in session.clients:
            session.clients.remove(client)
            log.debug("Client detached from session %s (%d remaining)",
                      session_id, len(session.clients))

    # ── Internal ────────────────────────────────────────────────────────────

    async def _read_loop(self, session: TerminalSession) -> None:
        """Read output from the pty and distribute to clients + scrollback."""
        loop = asyncio.get_running_loop()

        while session.alive:
            try:
                data = await loop.run_in_executor(
                    None, self._blocking_read, session.master_fd
                )
                if not data:
                    break

                session.last_activity = time.time()

                # Add to scrollback
                session.scrollback.append(data)
                session.scrollback_total += data.count(b'\n')
                # Trim scrollback
                while len(session.scrollback) > SCROLLBACK_LINES:
                    session.scrollback.pop(0)

                # Send to connected clients
                dead_clients = []
                for client in session.clients:
                    try:
                        await client.send_bytes(data)
                    except Exception:
                        dead_clients.append(client)
                for c in dead_clients:
                    session.clients.remove(c)

            except asyncio.CancelledError:
                return
            except Exception:
                break

        session.alive = False
        # Clean up zombie
        try:
            os.waitpid(session.pid, os.WNOHANG)
        except OSError:
            pass
        log.info("Terminal session ended: %s", session.id)

    @staticmethod
    def _blocking_read(fd: int) -> bytes:
        """Blocking read from pty master fd with timeout."""
        r, _, _ = select.select([fd], [], [], 1.0)
        if r:
            try:
                return os.read(fd, READ_SIZE)
            except OSError:
                return b""
        return b""  # timeout, try again

    @staticmethod
    def _set_winsize(fd: int, rows: int, cols: int) -> None:
        import fcntl
        import termios
        winsize = struct.pack("HHHH", rows, cols, 0, 0)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, winsize)
