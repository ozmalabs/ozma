# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Cross-desk clipboard ring — shared clipboard across all Desks and machines.

Copy on machine A at Desk 1 → paste on machine B at Desk 2.

The clipboard ring maintains a history of copied items, accessible from
any machine via the host agent or paste-as-typing.  In multi-Desk setups
(Grid), the ring is shared across all Desks.

Features:
  - Clipboard history (last 50 items)
  - Cross-machine paste (via host agent clipboard sync or paste-as-typing)
  - Cross-Desk paste (via Grid service)
  - Image support (text + images)
  - Pinned items (persistent, survive restart)
  - Search (find in clipboard history)
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections import deque
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.clipboard")

RING_SIZE = 50


@dataclass
class ClipboardEntry:
    """A single clipboard item."""
    id: int
    content: str
    content_type: str = "text"   # text, image, url
    source_node: str = ""        # Which machine it was copied from
    source_desk: str = ""        # Which Desk (for multi-Desk)
    timestamp: float = 0.0
    pinned: bool = False
    preview: str = ""            # First 100 chars for display

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "preview": self.preview or self.content[:100],
            "content_type": self.content_type,
            "source": self.source_node,
            "timestamp": self.timestamp,
            "pinned": self.pinned,
            "length": len(self.content),
        }


class ClipboardRing:
    """
    Shared clipboard ring across machines and Desks.
    """

    def __init__(self) -> None:
        self._entries: deque[ClipboardEntry] = deque(maxlen=RING_SIZE)
        self._counter = 0
        self._pinned: list[ClipboardEntry] = []

    def push(self, content: str, source_node: str = "",
             source_desk: str = "", content_type: str = "text") -> ClipboardEntry:
        """Add a new item to the clipboard ring."""
        # Don't duplicate the most recent entry
        if self._entries and self._entries[-1].content == content:
            return self._entries[-1]

        self._counter += 1
        entry = ClipboardEntry(
            id=self._counter,
            content=content[:65536],  # 64KB limit
            content_type=content_type,
            source_node=source_node,
            source_desk=source_desk,
            timestamp=time.time(),
            preview=content[:100].replace("\n", " "),
        )
        self._entries.append(entry)
        return entry

    def get(self, entry_id: int) -> ClipboardEntry | None:
        for e in self._entries:
            if e.id == entry_id:
                return e
        for e in self._pinned:
            if e.id == entry_id:
                return e
        return None

    def get_latest(self) -> ClipboardEntry | None:
        return self._entries[-1] if self._entries else None

    def list_entries(self, limit: int = 20) -> list[dict]:
        entries = list(self._entries)[-limit:]
        entries.reverse()
        pinned = [e.to_dict() for e in self._pinned]
        recent = [e.to_dict() for e in entries]
        return pinned + recent

    def search(self, query: str) -> list[dict]:
        query_lower = query.lower()
        matches = [e for e in self._entries if query_lower in e.content.lower()]
        return [e.to_dict() for e in matches[-20:]]

    def pin(self, entry_id: int) -> bool:
        entry = self.get(entry_id)
        if entry:
            entry.pinned = True
            if entry not in self._pinned:
                self._pinned.append(entry)
            return True
        return False

    def unpin(self, entry_id: int) -> bool:
        self._pinned = [e for e in self._pinned if e.id != entry_id]
        entry = self.get(entry_id)
        if entry:
            entry.pinned = False
        return True

    def clear(self) -> None:
        self._entries.clear()
