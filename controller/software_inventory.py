# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Fleet-wide software inventory collected from agent source adapters.

The controller stores the latest inventory snapshot for each host. Snapshots
are replaced as a unit so a partial or malformed upload can never leave a host
with a mix of old and new records.

Records follow the normalized shape defined by
``ozma-node/src/software/model.rs``. The controller deliberately accepts plain
dictionaries (and older agents that omit fields) so inventory ingestion does not
depend on the node binary being present.
"""

from __future__ import annotations

import copy
import json
import logging
import math
import re
import threading
import time
from collections import Counter
from collections.abc import Mapping
from enum import Enum
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

log = logging.getLogger("ozma.software_inventory")

DATA_DIR = Path(__file__).parent / "software_data"
INVENTORY_FILE = "inventory.json"

_KNOWN_RECORD_FIELDS = (
    "name",
    "source",
    "source_id",
    "version",
    "latest_version",
    "install_path",
    "scope",
    "user_id",
    "user_sid",
    "channel",
    "architecture",
    "managed_by",
    "update_state",
    "license_state",
    "identifiers",
    "metadata",
)

_VALID_SCOPES = frozenset({"system", "user", "provisioned", "unknown"})
_SCOPE_ALIASES = {
    "per_user": "user",
    "current_user": "user",
    "user_scope": "user",
    "machine": "system",
    "device": "system",
    "system_wide": "system",
    "systemwide": "system",
    "provisioned_package": "provisioned",
}

_VALID_UPDATE_STATES = frozenset(
    {"current", "outdated", "external_managed", "unknown", "unsupported"}
)
_UPDATE_STATE_ALIASES = {
    "uptodate": "current",
    "up_to_date": "current",
    "updated": "current",
    "latest": "current",
    "update_available": "outdated",
    "needs_update": "outdated",
    "pending_update": "outdated",
    "old": "outdated",
    "external": "external_managed",
    "externally_managed": "external_managed",
    "vendor_managed": "external_managed",
    "managed_externally": "external_managed",
    "not_supported": "unsupported",
    "notsupported": "unsupported",
}

_TRUE_STRINGS = frozenset({"1", "true", "yes", "y", "on"})
_FALSE_STRINGS = frozenset({"0", "false", "no", "n", "off"})


def _text(value: Any) -> str:
    """Return a stripped string, tolerating enums and missing values."""
    if value is None:
        return ""
    if isinstance(value, Enum):
        value = value.value
    if isinstance(value, str):
        return value.strip()
    return str(value).strip()


def _mapping(value: Any) -> dict[str, Any]:
    if isinstance(value, Mapping):
        return copy.deepcopy(dict(value))
    return {}


def _normalize_scope(value: Any) -> str:
    scope = _text(value).lower().replace("-", "_").replace(" ", "_")
    scope = _SCOPE_ALIASES.get(scope, scope)
    return scope if scope in _VALID_SCOPES else "unknown"


def _normalize_update_state(value: Any) -> str:
    state = _text(value).lower().replace("-", "_").replace(" ", "_")
    state = _UPDATE_STATE_ALIASES.get(state, state)
    return state if state in _VALID_UPDATE_STATES else "unknown"


def _normalize_managed_by(value: Any) -> str:
    managed_by = _text(value).lower().replace("-", "_").replace(" ", "_")
    return managed_by or "unknown"


def _normalize_source(value: Any) -> str:
    source = _text(value).lower().replace("-", "_").replace(" ", "_")
    return source or "unknown"


def _version_key(value: str) -> tuple[tuple[int, Any], ...]:
    """Natural-sort key so ``1.10`` sorts after ``1.9``."""
    parts: list[tuple[int, Any]] = []
    for part in re.split(r"(\d+)", value or ""):
        if not part:
            continue
        if part.isdigit():
            parts.append((0, int(part)))
        else:
            parts.append((1, part.lower()))
    return tuple(parts)


def _optional_bool(value: Any) -> bool | None:
    if value is None:
        return None
    if isinstance(value, bool):
        return value
    if isinstance(value, int):
        return bool(value)
    if isinstance(value, str):
        text = value.strip().lower()
        if text in _TRUE_STRINGS:
            return True
        if text in _FALSE_STRINGS:
            return False
    raise ValueError(f"invalid boolean value: {value!r}")


def _normalize_record(record: Mapping[str, Any]) -> dict[str, Any]:
    """Return one normalized record, preserving unknown fields in metadata."""
    normalized: dict[str, Any] = {
        "name": _text(record.get("name")),
        "source": _normalize_source(record.get("source")),
        "source_id": _text(record.get("source_id")),
        "version": _text(record.get("version")),
        "latest_version": _text(record.get("latest_version")),
        "install_path": _text(record.get("install_path")),
        "scope": _normalize_scope(record.get("scope")),
        "user_id": _text(record.get("user_id")),
        "user_sid": _text(record.get("user_sid")),
        "channel": _text(record.get("channel")),
        "architecture": _text(record.get("architecture")),
        "managed_by": _normalize_managed_by(record.get("managed_by")),
        "update_state": _normalize_update_state(record.get("update_state")),
        "license_state": _text(record.get("license_state")),
        "identifiers": _mapping(record.get("identifiers")),
        "metadata": _mapping(record.get("metadata")),
    }

    # Older and newer agents may send additional top-level fields. Keep them
    # without letting them alter the canonical fields above.
    for key, value in record.items():
        if key not in _KNOWN_RECORD_FIELDS:
            normalized["metadata"].setdefault(str(key), copy.deepcopy(value))

    return normalized


def _normalize_records(records: Any) -> list[dict[str, Any]]:
    if not isinstance(records, (list, tuple)):
        raise ValueError("records must be a list of dictionaries")
    normalized: list[dict[str, Any]] = []
    for index, record in enumerate(records):
        if not isinstance(record, Mapping):
            raise ValueError(f"record {index} must be a dictionary")
        normalized.append(_normalize_record(record))
    return normalized


def _counts(values: list[str]) -> dict[str, int]:
    return dict(sorted(Counter(values).items()))


def _summarize_records(records: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "records": len(records),
        "titles": len({(record["source"], record["source_id"] or record["name"])
                       for record in records}),
        "outdated": sum(1 for record in records
                        if record["update_state"] != "current"),
        "by_source": _counts([record["source"] for record in records]),
        "by_scope": _counts([record["scope"] for record in records]),
        "by_managed_by": _counts([record["managed_by"] for record in records]),
        "by_update_state": _counts([record["update_state"] for record in records]),
    }


class SoftwareInventoryManager:
    """Latest software inventory snapshot per host, with fleet views.

    Persistence is opt-in. Pass ``data_dir`` (tests usually use ``tmp_path``) or
    ``persist=True`` to write snapshots to ``software_data/inventory.json``.
    """

    def __init__(
        self,
        data_dir: Path | str | None = None,
        persist: bool | None = None,
    ) -> None:
        self._data_dir = Path(data_dir) if data_dir is not None else DATA_DIR
        self._persist = (data_dir is not None) if persist is None else bool(persist)
        self._lock = threading.RLock()
        self._hosts: dict[str, dict[str, Any]] = {}
        if self._persist:
            try:
                self._data_dir.mkdir(parents=True, exist_ok=True)
                self._load()
            except OSError as exc:
                log.warning("Software inventory persistence disabled: %s", exc)
                self._persist = False

    # ── Persistence ──────────────────────────────────────────────────────────

    @property
    def data_dir(self) -> Path:
        return self._data_dir

    def _load(self) -> None:
        path = self._data_dir / INVENTORY_FILE
        if not path.exists():
            return
        try:
            payload = json.loads(path.read_text())
            hosts = payload.get("hosts", {})
            if not isinstance(hosts, Mapping):
                raise ValueError("hosts must be an object")
            loaded: dict[str, dict[str, Any]] = {}
            for host_id, snapshot in hosts.items():
                if not isinstance(snapshot, Mapping):
                    continue
                records = _normalize_records(snapshot.get("records", []))
                reported_at = float(snapshot.get("reported_at", 0.0))
                loaded[str(host_id)] = {
                    "reported_at": reported_at,
                    "records": records,
                }
            self._hosts = loaded
        except (OSError, ValueError, TypeError, json.JSONDecodeError) as exc:
            log.warning("Failed to load software inventory: %s", exc)

    def _save(self) -> None:
        if not self._persist:
            return
        path = self._data_dir / INVENTORY_FILE
        tmp_path = path.with_suffix(".json.tmp")
        try:
            payload = {"version": 1, "hosts": self._hosts}
            tmp_path.write_text(json.dumps(payload, indent=2, sort_keys=True))
            tmp_path.replace(path)
        except (OSError, TypeError, ValueError) as exc:
            log.warning("Failed to save software inventory: %s", exc)

    # ── Ingestion ────────────────────────────────────────────────────────────

    def ingest(
        self,
        host_id: str,
        records: Any,
        reported_at: float | None = None,
    ) -> dict[str, Any]:
        """Replace one host's snapshot with a validated inventory upload.

        The snapshot is only assigned after every record has been validated and
        normalized, so a rejected upload leaves the previous snapshot intact.
        """
        normalized_host_id = _text(host_id)
        if not normalized_host_id:
            raise ValueError("host_id must be a non-empty string")

        normalized_records = _normalize_records(records)

        if reported_at is None:
            reported_timestamp = time.time()
        else:
            try:
                reported_timestamp = float(reported_at)
            except (TypeError, ValueError) as exc:
                raise ValueError("reported_at must be a number") from exc
            if not math.isfinite(reported_timestamp):
                raise ValueError("reported_at must be finite")

        snapshot = {
            "reported_at": reported_timestamp,
            "records": normalized_records,
        }
        with self._lock:
            self._hosts[normalized_host_id] = snapshot
            self._save()

        return {
            "host_id": normalized_host_id,
            "reported_at": reported_timestamp,
            "record_count": len(normalized_records),
            "records": copy.deepcopy(normalized_records),
            "summary": _summarize_records(normalized_records),
        }

    # ── Host views ───────────────────────────────────────────────────────────

    def host(self, host_id: str) -> dict[str, Any] | None:
        normalized_host_id = _text(host_id)
        with self._lock:
            snapshot = self._hosts.get(normalized_host_id)
            if snapshot is None:
                return None
            records = copy.deepcopy(snapshot["records"])
            return {
                "host_id": normalized_host_id,
                "reported_at": snapshot["reported_at"],
                "record_count": len(records),
                "records": records,
                "summary": _summarize_records(records),
            }

    def hosts(self) -> list[dict[str, Any]]:
        with self._lock:
            snapshots = list(self._hosts.items())
        result: list[dict[str, Any]] = []
        for host_id, snapshot in sorted(snapshots, key=lambda item: item[0]):
            records = copy.deepcopy(snapshot["records"])
            result.append({
                "host_id": host_id,
                "reported_at": snapshot["reported_at"],
                "record_count": len(records),
                "summary": _summarize_records(records),
            })
        return result

    # ── Fleet views ──────────────────────────────────────────────────────────

    def titles(
        self,
        source: str | None = None,
        outdated: bool | str | None = None,
    ) -> list[dict[str, Any]]:
        """Aggregate records into fleet titles keyed by source-qualified identity."""
        source_filter = _normalize_source(source) if source else None
        outdated_filter = _optional_bool(outdated)

        aggregated: dict[tuple[str, str], dict[str, Any]] = {}
        with self._lock:
            snapshots = list(self._hosts.items())

        for host_id, snapshot in snapshots:
            for record in snapshot["records"]:
                if source_filter is not None and record["source"] != source_filter:
                    continue
                identity = (record["source"], record["source_id"] or record["name"])
                title = aggregated.get(identity)
                if title is None:
                    title = {
                        "name": record["name"],
                        "source": record["source"],
                        "source_id": record["source_id"],
                        "versions": set(),
                        "latest_version": "",
                        "latest_key": None,
                        "hosts": set(),
                        "record_count": 0,
                        "update_state": Counter(),
                        "managed_by": Counter(),
                    }
                    aggregated[identity] = title

                if not title["name"] and record["name"]:
                    title["name"] = record["name"]
                if record["version"]:
                    title["versions"].add(record["version"])
                latest_version = record["latest_version"]
                if latest_version:
                    latest_key = _version_key(latest_version)
                    if title["latest_key"] is None or latest_key > title["latest_key"]:
                        title["latest_key"] = latest_key
                        title["latest_version"] = latest_version
                title["hosts"].add(host_id)
                title["record_count"] += 1
                title["update_state"][record["update_state"]] += 1
                title["managed_by"][record["managed_by"]] += 1

        titles: list[dict[str, Any]] = []
        for identity, title in aggregated.items():
            has_non_current = any(
                state != "current" for state in title["update_state"]
            )
            if outdated_filter is not None and has_non_current != outdated_filter:
                continue
            titles.append({
                "name": title["name"] or identity[1],
                "source": title["source"],
                "source_id": title["source_id"],
                "versions": sorted(title["versions"], key=_version_key),
                "latest_version": title["latest_version"],
                "host_count": len(title["hosts"]),
                "record_count": title["record_count"],
                "update_state": dict(sorted(title["update_state"].items())),
                "managed_by": dict(sorted(title["managed_by"].items())),
            })

        titles.sort(key=lambda item: (
            item["source"], item["name"].lower(), item["source_id"]
        ))
        return titles

    def outdated(self) -> dict[str, Any]:
        """Return non-current records grouped by ``managed_by``."""
        groups: dict[str, list[dict[str, Any]]] = {}
        with self._lock:
            snapshots = list(self._hosts.items())

        for host_id, snapshot in snapshots:
            for record in snapshot["records"]:
                if record["update_state"] == "current":
                    continue
                item = copy.deepcopy(record)
                item["host_id"] = host_id
                groups.setdefault(record["managed_by"], []).append(item)

        records: list[dict[str, Any]] = []
        for managed_by in sorted(groups):
            groups[managed_by].sort(key=lambda item: (
                item["source"], item["name"].lower(), item["source_id"]
            ))
            records.extend(groups[managed_by])

        return {
            "count": len(records),
            "managed_by": {key: groups[key] for key in sorted(groups)},
            "records": records,
        }

    def sources(self) -> list[dict[str, Any]]:
        """Return record/host counts and summaries for each source."""
        aggregates: dict[str, dict[str, Any]] = {}
        with self._lock:
            snapshots = list(self._hosts.items())

        for host_id, snapshot in snapshots:
            for record in snapshot["records"]:
                source = record["source"]
                aggregate = aggregates.get(source)
                if aggregate is None:
                    aggregate = {
                        "source": source,
                        "records": 0,
                        "hosts": set(),
                        "managed_by": Counter(),
                        "update_state": Counter(),
                    }
                    aggregates[source] = aggregate
                aggregate["records"] += 1
                aggregate["hosts"].add(host_id)
                aggregate["managed_by"][record["managed_by"]] += 1
                aggregate["update_state"][record["update_state"]] += 1

        result: list[dict[str, Any]] = []
        for source in sorted(aggregates):
            aggregate = aggregates[source]
            result.append({
                "source": source,
                "records": aggregate["records"],
                "hosts": len(aggregate["hosts"]),
                "managed_by": dict(sorted(aggregate["managed_by"].items())),
                "update_state": dict(sorted(aggregate["update_state"].items())),
            })
        return result

    def summary(self) -> dict[str, Any]:
        with self._lock:
            snapshots = list(self._hosts.items())

        records = [
            record
            for _host_id, snapshot in snapshots
            for record in snapshot["records"]
        ]
        source_counts = Counter(record["source"] for record in records)
        update_states = [record["update_state"] for record in records]
        reported_times = [
            snapshot["reported_at"] for _host_id, snapshot in snapshots
        ]

        return {
            "hosts": len(snapshots),
            "records": len(records),
            "titles": len({
                (record["source"], record["source_id"] or record["name"])
                for record in records
            }),
            "sources": len(source_counts),
            "outdated": sum(1 for state in update_states if state != "current"),
            "by_source": dict(sorted(source_counts.items())),
            "by_update_state": _counts(update_states),
            "by_managed_by": _counts([
                record["managed_by"] for record in records
            ]),
            "last_reported_at": max(reported_times) if reported_times else None,
        }


class InventoryPayload(BaseModel):
    """Body for ``POST /api/v1/software/hosts/{host_id}/inventory``."""

    records: list[dict[str, Any]]
    reported_at: float | None = None


def build_router(manager: SoftwareInventoryManager) -> APIRouter:
    """Build the fleet software-inventory API around one manager instance."""
    router = APIRouter(tags=["software"])

    @router.post("/api/v1/software/hosts/{host_id}/inventory")
    def ingest_inventory(host_id: str, payload: InventoryPayload) -> dict[str, Any]:
        try:
            snapshot = manager.ingest(
                host_id, payload.records, reported_at=payload.reported_at
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"ok": True, **snapshot}

    # Register the collection route before the item route so ``/hosts`` is not
    # captured as a host named "hosts".
    @router.get("/api/v1/software/hosts")
    def list_hosts() -> dict[str, Any]:
        hosts = manager.hosts()
        return {"hosts": hosts, "count": len(hosts)}

    @router.get("/api/v1/software/hosts/{host_id}")
    def get_host(host_id: str) -> dict[str, Any]:
        snapshot = manager.host(host_id)
        if snapshot is None:
            raise HTTPException(status_code=404, detail=f"Unknown host {host_id!r}")
        return snapshot

    @router.get("/api/v1/software/titles")
    def list_titles(
        source: str | None = None,
        outdated: bool | None = None,
    ) -> dict[str, Any]:
        try:
            titles = manager.titles(source=source, outdated=outdated)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"titles": titles, "count": len(titles)}

    @router.get("/api/v1/software/outdated")
    def list_outdated() -> dict[str, Any]:
        return manager.outdated()

    @router.get("/api/v1/software/sources")
    def list_sources() -> dict[str, Any]:
        sources = manager.sources()
        return {"sources": sources, "count": len(sources)}

    return router
