# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
PipelineCache — caches Router.recommend() results and invalidates on graph changes.

When a node joins or leaves, or the active node changes, the routing graph
changes. The cache is invalidated and recomputed lazily on the next query.

Keys are (intent_name, source_PortRef, destination_PortRef, top_n).
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .intent import Intent, StreamIntent
    from .model import PortRef
    from .pipeline import Pipeline
    from .router import Router

log = logging.getLogger("ozma.routing.pipeline_cache")


@dataclass
class CacheEntry:
    recommendations: list[tuple]   # list[(StreamIntent, list[Pipeline])]
    computed_at: float = field(default_factory=time.monotonic)
    generation: int = 0


class PipelineCache:
    """
    Lazy cache for Router.recommend() results.

    - `invalidate()` bumps the generation counter and clears cached results.
    - `get_or_compute()` returns a cached result if the generation matches,
      otherwise calls router.recommend() and stores the result.
    - `all_entries()` returns all currently cached recommendations.
    """

    def __init__(self, router: Router) -> None:
        self._router = router
        self._cache: dict[tuple, CacheEntry] = {}
        self._generation: int = 0

    @property
    def generation(self) -> int:
        return self._generation

    def invalidate(self) -> None:
        """Clear all cached results (e.g. after graph topology changes)."""
        self._cache.clear()
        self._generation += 1
        log.debug("Pipeline cache invalidated (generation %d)", self._generation)

    def get_or_compute(
        self,
        intent: Intent,
        source: PortRef,
        destination: PortRef,
        top_n: int = 3,
    ) -> list[tuple[StreamIntent, list[Pipeline]]]:
        """
        Return cached recommendations or compute fresh ones.

        The cache key is (intent.name, source, destination, top_n). Since
        PortRef defines __hash__ and __eq__, it works as a dict key.
        """
        key = (intent.name, source, destination, top_n)
        entry = self._cache.get(key)
        if entry is not None and entry.generation == self._generation:
            return entry.recommendations

        t0 = time.monotonic()
        recommendations = self._router.recommend(intent, source, destination, top_n)
        elapsed = time.monotonic() - t0
        log.debug(
            "Pipeline cache computed %s %s→%s in %.1fms (gen %d)",
            intent.name, source.device_id, destination.device_id,
            elapsed * 1000, self._generation,
        )
        self._cache[key] = CacheEntry(
            recommendations=recommendations,
            generation=self._generation,
        )
        return recommendations

    def all_entries(self) -> list[dict]:
        """
        Return all currently cached pipeline sets as dicts for the API.

        Each dict has: intent, source, destination, streams (list of per-stream
        pipeline candidates), computed_at, generation.
        """
        result = []
        for (intent_name, source, dest, top_n), entry in self._cache.items():
            if entry.generation != self._generation:
                continue  # stale — skip
            streams = []
            for stream_intent, pipelines in entry.recommendations:
                streams.append({
                    "media_type": stream_intent.media_type.value,
                    "required": stream_intent.required,
                    "pipelines": [p.to_dict() for p in pipelines],
                })
            result.append({
                "intent": intent_name,
                "source": source.to_dict(),
                "destination": dest.to_dict(),
                "top_n": top_n,
                "streams": streams,
                "computed_at": entry.computed_at,
                "generation": entry.generation,
            })
        return result

    def __len__(self) -> int:
        return sum(
            1 for e in self._cache.values() if e.generation == self._generation
        )

    def to_dict(self) -> dict:
        return {
            "generation": self._generation,
            "cached_entries": len(self),
            "entries": self.all_entries(),
        }
