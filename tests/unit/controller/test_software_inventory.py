# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the controller-side fleet software inventory service."""

from __future__ import annotations

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from software_inventory import SoftwareInventoryManager, build_router

pytestmark = pytest.mark.unit


def _record(**overrides):
    record = {
        "name": "Example App",
        "source": "apt",
        "source_id": "example-app",
        "version": "1.0.0",
        "latest_version": "1.1.0",
        "install_path": "/usr/lib/example-app",
        "scope": "system",
        "managed_by": "ozma",
        "update_state": "current",
    }
    record.update(overrides)
    return record


def _client(manager):
    app = FastAPI()
    app.include_router(build_router(manager))
    return TestClient(app)


# ── Manager: ingestion and host snapshots ────────────────────────────────────


def test_ingest_replaces_host_snapshot_atomically():
    manager = SoftwareInventoryManager()

    first = manager.ingest("host-a", [_record()], reported_at=100.0)
    assert first["host_id"] == "host-a"
    assert first["reported_at"] == 100.0
    assert first["record_count"] == 1

    second = manager.ingest(
        "host-a",
        [_record(name="Replacement", source_id="replacement")],
        reported_at=200.0,
    )
    assert second["record_count"] == 1

    snapshot = manager.host("host-a")
    assert snapshot is not None
    assert snapshot["reported_at"] == 200.0
    assert [record["name"] for record in snapshot["records"]] == ["Replacement"]
    assert snapshot["summary"]["records"] == 1
    assert manager.host("missing") is None
    assert [host["host_id"] for host in manager.hosts()] == ["host-a"]


def test_ingest_rejects_bad_payload_without_replacing_existing_snapshot():
    manager = SoftwareInventoryManager()
    manager.ingest("host-a", [_record()], reported_at=10.0)

    with pytest.raises(ValueError):
        manager.ingest("", [_record()])
    with pytest.raises(ValueError):
        manager.ingest("host-a", "not-a-list")
    with pytest.raises(ValueError):
        manager.ingest("host-a", [42])
    with pytest.raises(ValueError):
        manager.ingest("host-a", [_record()], reported_at="later")
    with pytest.raises(ValueError):
        manager.ingest("host-a", [_record()], reported_at=float("nan"))

    snapshot = manager.host("host-a")
    assert snapshot is not None
    assert snapshot["reported_at"] == 10.0
    assert snapshot["records"][0]["name"] == "Example App"


def test_hosts_returns_summaries_without_record_payloads():
    manager = SoftwareInventoryManager()
    manager.ingest("host-b", [_record(), _record(source_id="second")], reported_at=2.0)
    manager.ingest("host-a", [_record(source="steam", source_id="10")], reported_at=1.0)

    hosts = manager.hosts()
    assert [host["host_id"] for host in hosts] == ["host-a", "host-b"]
    assert hosts[0]["record_count"] == 1
    assert hosts[1]["summary"]["records"] == 2
    assert hosts[1]["summary"]["by_source"] == {"apt": 2}
    assert "records" not in hosts[0]


def test_missing_fields_and_unknown_values_are_normalized():
    manager = SoftwareInventoryManager()
    manager.ingest(
        "legacy-host",
        [
            {
                "name": "Legacy",
                "source": "Steam",
                "scope": "PER-USER",
                "update_state": "External-Managed",
                "future_field": "kept",
                "identifiers": ["not", "a", "mapping"],
            },
            {
                "name": "Unknowns",
                "scope": "galactic",
                "update_state": "definitely-not-a-state",
            },
        ],
    )

    snapshot = manager.host("legacy-host")
    assert snapshot is not None
    first, second = snapshot["records"]

    assert first["source"] == "steam"
    assert first["scope"] == "user"
    assert first["update_state"] == "external_managed"
    assert first["managed_by"] == "unknown"
    assert first["user_sid"] == ""
    assert first["identifiers"] == {}
    assert first["metadata"]["future_field"] == "kept"

    assert second["source"] == "unknown"
    assert second["scope"] == "unknown"
    assert second["update_state"] == "unknown"


# ── Manager: fleet views ─────────────────────────────────────────────────────


def test_titles_are_qualified_by_source_and_source_id():
    manager = SoftwareInventoryManager()
    manager.ingest(
        "host-a",
        [
            _record(name="Steam Game", source="steam", source_id="10",
                    version="1.0", latest_version=""),
        ],
    )
    manager.ingest(
        "host-b",
        [
            _record(name="Steam Game", source="steam", source_id="10",
                    version="2.0", latest_version="2.1"),
            _record(name="Steam Game", source="apt", source_id="steam-game"),
            _record(name="Steam Game", source="steam", source_id="11"),
        ],
    )

    titles = manager.titles()
    assert len(titles) == 3

    steam_10 = next(
        title for title in titles
        if title["source"] == "steam" and title["source_id"] == "10"
    )
    assert steam_10["name"] == "Steam Game"
    assert steam_10["host_count"] == 2
    assert steam_10["record_count"] == 2
    assert steam_10["versions"] == ["1.0", "2.0"]
    assert steam_10["latest_version"] == "2.1"
    assert steam_10["update_state"] == {"current": 2}

    assert [title["source"] for title in manager.titles(source="steam")] == [
        "steam", "steam",
    ]
    assert len(manager.titles(source="apt")) == 1


def test_titles_dedupe_hosts_and_natural_sort_versions():
    manager = SoftwareInventoryManager()
    manager.ingest(
        "host-a",
        [
            _record(version="1.9"),
            _record(version="1.10"),
        ],
    )

    title = manager.titles()[0]
    assert title["host_count"] == 1
    assert title["record_count"] == 2
    assert title["versions"] == ["1.9", "1.10"]


def test_titles_outdated_filter_uses_non_current_states():
    manager = SoftwareInventoryManager()
    manager.ingest(
        "host-a",
        [
            _record(name="Current", source_id="current"),
            _record(name="Old", source_id="old", update_state="outdated"),
            _record(name="Vendor", source_id="vendor",
                    update_state="external_managed"),
        ],
    )

    outdated = manager.titles(outdated=True)
    assert {title["name"] for title in outdated} == {"Old", "Vendor"}

    current = manager.titles(outdated=False)
    assert [title["name"] for title in current] == ["Current"]

    assert len(manager.titles(outdated="true")) == 2
    assert len(manager.titles(outdated="false")) == 1
    with pytest.raises(ValueError):
        manager.titles(outdated="sometimes")


def test_sources_and_summary_report_counts():
    manager = SoftwareInventoryManager()
    manager.ingest(
        "host-a",
        [
            _record(source="apt", source_id="one", managed_by="ozma"),
            _record(source="apt", source_id="two", managed_by="ozma",
                    update_state="outdated"),
            _record(source="steam", source_id="10", managed_by="steam",
                    update_state="external_managed"),
        ],
        reported_at=50.0,
    )
    manager.ingest(
        "host-b",
        [_record(source="apt", source_id="one", managed_by="ozma")],
        reported_at=75.0,
    )

    sources = manager.sources()
    assert [source["source"] for source in sources] == ["apt", "steam"]

    apt = sources[0]
    assert apt["records"] == 3
    assert apt["hosts"] == 2
    assert apt["managed_by"] == {"ozma": 3}
    assert apt["update_state"] == {"current": 2, "outdated": 1}

    steam = sources[1]
    assert steam["records"] == 1
    assert steam["hosts"] == 1
    assert steam["update_state"] == {"external_managed": 1}

    summary = manager.summary()
    assert summary["hosts"] == 2
    assert summary["records"] == 4
    assert summary["titles"] == 3
    assert summary["sources"] == 2
    assert summary["outdated"] == 2
    assert summary["by_source"] == {"apt": 3, "steam": 1}
    assert summary["by_managed_by"] == {"ozma": 3, "steam": 1}
    assert summary["by_update_state"] == {
        "current": 2, "external_managed": 1, "outdated": 1,
    }
    assert summary["last_reported_at"] == 75.0


def test_outdated_groups_non_current_records_by_managed_by():
    manager = SoftwareInventoryManager()
    manager.ingest(
        "host-a",
        [
            _record(name="Up to date", source_id="current"),
            _record(name="Ozma update", source_id="ozma-outdated",
                    managed_by="ozma", update_state="outdated"),
            _record(name="Steam game", source="steam", source_id="10",
                    managed_by="steam", update_state="external_managed"),
            _record(name="Unknown app", source_id="unknown",
                    managed_by="ozma", update_state="unknown"),
        ],
    )
    manager.ingest(
        "host-b",
        [_record(name="Other host", source_id="other",
                 managed_by="vendor", update_state="unsupported")],
    )

    result = manager.outdated()
    assert result["count"] == 4
    assert set(result["managed_by"]) == {"ozma", "steam", "vendor"}
    assert len(result["managed_by"]["ozma"]) == 2
    assert result["managed_by"]["steam"][0]["host_id"] == "host-a"
    assert result["managed_by"]["vendor"][0]["update_state"] == "unsupported"
    assert len(result["records"]) == 4
    assert all(record["update_state"] != "current" for record in result["records"])


def test_persistence_round_trip(tmp_path):
    first = SoftwareInventoryManager(data_dir=tmp_path)
    first.ingest(
        "host-a",
        [_record(name="Persisted", source_id="persisted")],
        reported_at=123.0,
    )
    assert (tmp_path / "inventory.json").exists()

    second = SoftwareInventoryManager(data_dir=tmp_path)
    snapshot = second.host("host-a")
    assert snapshot is not None
    assert snapshot["reported_at"] == 123.0
    assert snapshot["records"][0]["name"] == "Persisted"
    assert second.summary()["hosts"] == 1


# ── Router ───────────────────────────────────────────────────────────────────


def test_router_ingest_and_host_views():
    manager = SoftwareInventoryManager()
    client = _client(manager)

    response = client.post(
        "/api/v1/software/hosts/host-a/inventory",
        json={"records": [_record()], "reported_at": 100.0},
    )
    assert response.status_code == 200, response.text
    payload = response.json()
    assert payload["ok"] is True
    assert payload["host_id"] == "host-a"
    assert payload["record_count"] == 1
    assert payload["reported_at"] == 100.0

    response = client.get("/api/v1/software/hosts/host-a")
    assert response.status_code == 200
    assert response.json()["records"][0]["name"] == "Example App"

    response = client.get("/api/v1/software/hosts")
    assert response.status_code == 200
    hosts = response.json()
    assert hosts["count"] == 1
    assert hosts["hosts"][0]["host_id"] == "host-a"


def test_router_fleet_views():
    manager = SoftwareInventoryManager()
    manager.ingest(
        "host-a",
        [
            _record(source="apt", source_id="one"),
            _record(source="steam", source_id="10", managed_by="steam",
                    update_state="external_managed"),
            _record(source="apt", source_id="two", update_state="outdated"),
        ],
    )
    client = _client(manager)

    response = client.get("/api/v1/software/titles")
    assert response.status_code == 200
    assert response.json()["count"] == 3

    response = client.get("/api/v1/software/titles", params={"source": "steam"})
    assert response.status_code == 200
    assert [title["source"] for title in response.json()["titles"]] == ["steam"]

    response = client.get("/api/v1/software/titles", params={"outdated": "true"})
    assert response.status_code == 200
    assert response.json()["count"] == 2

    response = client.get("/api/v1/software/outdated")
    assert response.status_code == 200
    assert response.json()["count"] == 2
    assert set(response.json()["managed_by"]) == {"ozma", "steam"}

    response = client.get("/api/v1/software/sources")
    assert response.status_code == 200
    assert response.json()["count"] == 2


def test_router_unknown_host_returns_404():
    client = _client(SoftwareInventoryManager())

    response = client.get("/api/v1/software/hosts/missing")
    assert response.status_code == 404
    assert "Unknown host" in response.json()["detail"]


def test_router_rejects_malformed_bodies():
    client = _client(SoftwareInventoryManager())

    assert client.post(
        "/api/v1/software/hosts/host-a/inventory",
        json={},
    ).status_code == 422
    assert client.post(
        "/api/v1/software/hosts/host-a/inventory",
        json={"records": "not-a-list"},
    ).status_code == 422
    assert client.post(
        "/api/v1/software/hosts/host-a/inventory",
        json={"records": [42]},
    ).status_code == 422
    assert client.post(
        "/api/v1/software/hosts/host-a/inventory",
        json={"records": [], "reported_at": "soon"},
    ).status_code == 422
    assert client.get(
        "/api/v1/software/titles", params={"outdated": "maybe"}
    ).status_code == 422


def test_router_maps_manager_validation_errors_to_400(monkeypatch):
    manager = SoftwareInventoryManager()
    client = _client(manager)

    def _reject(*_args, **_kwargs):
        raise ValueError("invalid inventory")

    monkeypatch.setattr(manager, "ingest", _reject)
    response = client.post(
        "/api/v1/software/hosts/host-a/inventory",
        json={"records": [_record()]},
    )
    assert response.status_code == 400
    assert response.json()["detail"] == "invalid inventory"
