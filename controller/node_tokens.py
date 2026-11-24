# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Persistent per-node bearer-token store.

The controller keeps a small JSON file mapping node ids to the bearer token the
node's HTTP API expects. It survives controller restarts and takes precedence
over a token supplied at registration, so an operator can rotate a token without
editing node configuration on every re-registration.

File format (``node_tokens.json``, mode ``0600``)::

    {
      "version": 1,
      "nodes": {
        "node-id._ozma._udp.local.": {
          "token": "<bearer token>",
          "source": "registration" | "rotation" | "manual",
          "updated_at": 1764028800.0
        }
      },
      "revoked": ["node-id-with-a-revocation-tombstone._ozma._udp.local."]
    }

``revoked`` lists nodes whose token was explicitly cleared. A tombstone stops a
later re-registration from re-adopting the old token; an explicit ``set()`` or
``forget()`` removes it.

Tokens are secrets: this module never logs, serializes into API responses, or
includes them in metadata returned by :meth:`NodeTokenStore.metadata`. The file
is written atomically (same-directory temp file + ``os.replace``) with mode
``0600``. At-rest encryption is not provided; protect the controller data
directory.

Path selection, in order: an explicit ``path`` argument, ``OZMA_NODE_TOKEN_STORE``,
then ``<controller dir>/node_tokens.json``.
"""

from __future__ import annotations

import json
import logging
import os
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.node_tokens")

STORE_VERSION = 1
DEFAULT_FILENAME = "node_tokens.json"


def default_store_path() -> Path:
    """Return the configured or default token-store path."""
    env_path = os.environ.get("OZMA_NODE_TOKEN_STORE", "").strip()
    if env_path:
        return Path(env_path).expanduser()
    return Path(__file__).resolve().parent / DEFAULT_FILENAME


class NodeTokenStore:
    """JSON-backed per-node token store with atomic, permissioned writes."""

    def __init__(self, path: str | os.PathLike[str] | None = None) -> None:
        self._path = Path(path).expanduser() if path else default_store_path()
        self._lock = threading.RLock()
        self._nodes: dict[str, dict[str, Any]] = {}
        # Revoked node ids are remembered so a re-registration cannot silently
        # re-adopt the old token. Explicit cleanup (forget) removes them.
        self._revoked: set[str] = set()
        self._load()

    @property
    def path(self) -> Path:
        return self._path

    # ── Persistence ───────────────────────────────────────────────────────────

    def _load(self) -> None:
        """Load tokens from disk, ignoring malformed entries."""
        try:
            raw = self._path.read_text()
        except FileNotFoundError:
            return
        except OSError as exc:
            log.warning("Could not read node token store %s: %s", self._path.name, exc)
            return
        try:
            data = json.loads(raw)
        except json.JSONDecodeError:
            log.warning("Ignoring malformed node token store %s", self._path.name)
            return

        nodes = data.get("nodes") if isinstance(data, dict) else None
        if not isinstance(nodes, dict):
            # Tolerate a bare ``{node_id: {...}}`` map from manual edits.
            nodes = data if isinstance(data, dict) else {}

        loaded: dict[str, dict[str, Any]] = {}
        for node_id, entry in nodes.items():
            if not isinstance(node_id, str) or not isinstance(entry, dict):
                continue
            token = str(entry.get("token", "") or "").strip()
            if not token:
                continue
            loaded[node_id] = {
                "token": token,
                "source": str(entry.get("source", "manual") or "manual"),
                "updated_at": float(entry.get("updated_at", 0) or 0),
            }
        revoked_raw = data.get("revoked", []) if isinstance(data, dict) else []
        revoked = {
            str(node_id) for node_id in revoked_raw if isinstance(node_id, str)
        } if isinstance(revoked_raw, list) else set()
        self._nodes = loaded
        self._revoked = revoked - set(loaded)
        if loaded:
            log.info("Loaded %d node token(s) from %s", len(loaded), self._path.name)
        self._ensure_private_permissions()

    def _ensure_private_permissions(self) -> None:
        """Best-effort tighten an existing store to owner-only."""
        try:
            if self._path.exists():
                mode = self._path.stat().st_mode & 0o777
                if mode != 0o600:
                    os.chmod(self._path, 0o600)
        except OSError as exc:
            log.warning("Could not restrict node token store permissions: %s", exc)

    def _save(self) -> None:
        """Write the store atomically with mode 0600."""
        payload = {
            "version": STORE_VERSION,
            "nodes": self._nodes,
            "revoked": sorted(self._revoked),
        }
        self._path.parent.mkdir(parents=True, exist_ok=True)
        fd, tmp_name = tempfile.mkstemp(
            prefix=f".{self._path.name}.", dir=str(self._path.parent)
        )
        try:
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                json.dump(payload, handle, indent=2, sort_keys=True)
                handle.write("\n")
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(tmp_name, self._path)
            os.chmod(self._path, 0o600)
        except OSError as exc:
            log.warning("Could not save node token store %s: %s", self._path.name, exc)
            try:
                os.unlink(tmp_name)
            except OSError:
                pass

    # ── Queries ───────────────────────────────────────────────────────────────

    def get(self, node_id: str) -> str:
        """Return a node's stored token, or ``""`` when none is active."""
        with self._lock:
            if node_id in self._revoked:
                return ""
            entry = self._nodes.get(node_id)
            return str(entry.get("token", "") if entry else "")

    def has(self, node_id: str) -> bool:
        return bool(self.get(node_id))

    def is_revoked(self, node_id: str) -> bool:
        with self._lock:
            return node_id in self._revoked

    def metadata(self, node_id: str) -> dict[str, Any]:
        """Return token-free metadata for API responses."""
        with self._lock:
            if node_id in self._revoked:
                return {
                    "node_id": node_id,
                    "configured": False,
                    "revoked": True,
                    "source": None,
                    "updated_at": None,
                }
            entry = self._nodes.get(node_id)
            if not entry:
                return {
                    "node_id": node_id,
                    "configured": False,
                    "revoked": False,
                    "source": None,
                    "updated_at": None,
                }
            return {
                "node_id": node_id,
                "configured": True,
                "revoked": False,
                "source": entry.get("source", "manual"),
                "updated_at": entry.get("updated_at") or None,
            }

    def node_ids(self) -> list[str]:
        with self._lock:
            return sorted(self._nodes)

    # ── Mutations ─────────────────────────────────────────────────────────────

    def set(self, node_id: str, token: str, *, source: str = "manual") -> bool:
        """Store (or rotate) a token. Returns ``True`` when the store changed."""
        token = str(token or "").strip()
        if not node_id or not token:
            return False
        with self._lock:
            current = self._nodes.get(node_id)
            if (
                current
                and current.get("token") == token
                and current.get("source") == source
                and node_id not in self._revoked
            ):
                return False
            self._revoked.discard(node_id)
            self._nodes[node_id] = {
                "token": token,
                "source": source,
                "updated_at": time.time(),
            }
            self._save()
            log.info("Stored node token for %s (source=%s)", node_id, source)
            return True

    def clear(self, node_id: str) -> bool:
        """Revoke a node's token with a tombstone.

        Returns ``True`` when state changed. The tombstone stops a later
        re-registration from re-adopting the old token; use :meth:`forget` on
        explicit node removal to delete both the token and the tombstone.
        """
        with self._lock:
            had_token = node_id in self._nodes
            already_revoked = node_id in self._revoked
            if not had_token and already_revoked:
                return False
            self._nodes.pop(node_id, None)
            self._revoked.add(node_id)
            self._save()
            log.info("Revoked node token for %s", node_id)
            return True

    def forget(self, node_id: str) -> bool:
        """Delete a node's token and revocation tombstone (hard cleanup)."""
        with self._lock:
            had_token = node_id in self._nodes
            was_revoked = node_id in self._revoked
            if not had_token and not was_revoked:
                return False
            self._nodes.pop(node_id, None)
            self._revoked.discard(node_id)
            self._save()
            log.info("Forgot node token for %s", node_id)
            return True

    def reload(self) -> None:
        """Re-read the store from disk (used by tests and manual edits)."""
        with self._lock:
            self._nodes = {}
            self._revoked = set()
            self._load()
