# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for controller.gaming.scenario_app_mapping."""

from __future__ import annotations

import pytest

from controller.gaming.scenario_app_mapping import (
    AppSource,
    MoonlightApp,
    ScenarioAppMapper,
    SourceType,
    _infer_source_type,
    _stable_app_id,
)


# ── Helpers ───────────────────────────────────────────────────────────────────

def _scenario(id: str, **kwargs) -> dict:
    return {"id": id, "name": id.replace("-", " ").title(), **kwargs}


# ── _stable_app_id ────────────────────────────────────────────────────────────

class TestStableAppId:
    def test_positive(self):
        assert _stable_app_id("foo") > 0

    def test_fits_signed_32bit(self):
        assert _stable_app_id("foo") <= 0x7FFF_FFFF

    def test_stable_across_calls(self):
        assert _stable_app_id("my-scenario") == _stable_app_id("my-scenario")

    def test_different_ids_differ(self):
        assert _stable_app_id("a") != _stable_app_id("b")


# ── _infer_source_type ────────────────────────────────────────────────────────

class TestInferSourceType:
    def test_explicit_hdmi(self):
        s = _scenario("x", streaming_source="hdmi_capture")
        assert _infer_source_type(s) == SourceType.HDMI_CAPTURE

    def test_explicit_vnc(self):
        s = _scenario("x", streaming_source="vnc")
        assert _infer_source_type(s) == SourceType.VNC

    def test_explicit_virtual_desktop(self):
        s = _scenario("x", streaming_source="virtual_desktop")
        assert _infer_source_type(s) == SourceType.VIRTUAL_DESKTOP

    def test_explicit_sunshine(self):
        s = _scenario("x", streaming_source="sunshine")
        assert _infer_source_type(s) == SourceType.SUNSHINE

    def test_explicit_unknown_value_falls_through(self):
        # Bad explicit value → falls through to heuristics → UNKNOWN
        s = _scenario("x", streaming_source="bogus")
        assert _infer_source_type(s) == SourceType.UNKNOWN

    def test_capture_source_key(self):
        s = _scenario("x", capture_source="hdmi-0")
        assert _infer_source_type(s) == SourceType.HDMI_CAPTURE

    def test_vnc_host_key(self):
        s = _scenario("x", vnc_host="192.168.1.10")
        assert _infer_source_type(s) == SourceType.VNC

    def test_vm_id_key(self):
        s = _scenario("x", vm_id="vm-42")
        assert _infer_source_type(s) == SourceType.VNC

    def test_container_id_key(self):
        s = _scenario("x", container_id="ctr-1")
        assert _infer_source_type(s) == SourceType.VIRTUAL_DESKTOP

    def test_wayland_display_key(self):
        s = _scenario("x", wayland_display="wayland-1")
        assert _infer_source_type(s) == SourceType.VIRTUAL_DESKTOP

    def test_sunshine_host_key(self):
        s = _scenario("x", sunshine_host="10.0.0.5")
        assert _infer_source_type(s) == SourceType.SUNSHINE

    def test_no_hints_returns_unknown(self):
        s = _scenario("x")
        assert _infer_source_type(s) == SourceType.UNKNOWN

    def test_capture_source_takes_priority_over_vnc(self):
        # Both keys present — capture_source wins
        s = _scenario("x", capture_source="hdmi-0", vnc_host="10.0.0.1")
        assert _infer_source_type(s) == SourceType.HDMI_CAPTURE


# ── ScenarioAppMapper.refresh ─────────────────────────────────────────────────

class TestRefresh:
    def test_empty_list(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([])
        assert mapper.build_app_list() == []

    def test_single_scenario(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([_scenario("work", capture_source="hdmi-0")])
        apps = mapper.build_app_list()
        assert len(apps) == 1
        assert apps[0]["AppTitle"] == "Work"

    def test_streamable_false_excluded(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([
            _scenario("visible"),
            _scenario("hidden", streamable=False),
        ])
        assert len(mapper.build_app_list()) == 1
        assert mapper.build_app_list()[0]["AppTitle"] == "Visible"

    def test_scenario_without_id_skipped(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([{"name": "No ID"}])
        assert mapper.build_app_list() == []

    def test_sorted_alphabetically(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([
            _scenario("zebra"),
            _scenario("alpha"),
            _scenario("middle"),
        ])
        titles = [a["AppTitle"] for a in mapper.build_app_list()]
        assert titles == sorted(titles, key=str.lower)

    def test_refresh_replaces_previous(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([_scenario("old")])
        mapper.refresh([_scenario("new")])
        titles = [a["AppTitle"] for a in mapper.build_app_list()]
        assert titles == ["New"]

    def test_stable_id_preserved_across_refresh(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([_scenario("stable-id")])
        id1 = mapper.build_app_list()[0]["ID"]
        mapper.refresh([_scenario("stable-id")])
        id2 = mapper.build_app_list()[0]["ID"]
        assert id1 == id2

    def test_hdr_flag(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([_scenario("hdr-scene", hdr=True)])
        assert mapper.build_app_list()[0]["IsHdrSupported"] == 1

    def test_no_hdr_by_default(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([_scenario("plain")])
        assert mapper.build_app_list()[0]["IsHdrSupported"] == 0


# ── ScenarioAppMapper lookups ─────────────────────────────────────────────────

class TestLookups:
    def _mapper_with(self, *scenarios) -> ScenarioAppMapper:
        mapper = ScenarioAppMapper()
        mapper.refresh(list(scenarios))
        return mapper

    def test_get_app_by_scenario(self):
        mapper = self._mapper_with(_scenario("s1", capture_source="hdmi-0"))
        app = mapper.get_app_by_scenario("s1")
        assert app is not None
        assert app.scenario_id == "s1"
        assert app.source_type == SourceType.HDMI_CAPTURE

    def test_get_app_by_scenario_missing(self):
        mapper = self._mapper_with(_scenario("s1"))
        assert mapper.get_app_by_scenario("nope") is None

    def test_get_app_by_id(self):
        mapper = self._mapper_with(_scenario("s1"))
        numeric_id = mapper.build_app_list()[0]["ID"]
        app = mapper.get_app_by_id(numeric_id)
        assert app is not None
        assert app.scenario_id == "s1"

    def test_get_app_by_id_missing(self):
        mapper = self._mapper_with(_scenario("s1"))
        assert mapper.get_app_by_id(999_999_999) is None

    def test_get_scenario_id(self):
        mapper = self._mapper_with(_scenario("s1"))
        numeric_id = mapper.build_app_list()[0]["ID"]
        assert mapper.get_scenario_id(numeric_id) == "s1"

    def test_resolve_source_unknown_id(self):
        mapper = self._mapper_with(_scenario("s1"))
        assert mapper.resolve_source(999_999_999) is None

    def test_resolve_source_returns_source_type(self):
        mapper = self._mapper_with(_scenario("s1", vnc_host="10.0.0.1"))
        numeric_id = mapper.build_app_list()[0]["ID"]
        src = mapper.resolve_source(numeric_id)
        assert src is not None
        assert src.source_type == SourceType.VNC

    def test_resolve_source_for_scenario_hdmi(self):
        mapper = ScenarioAppMapper()
        s = _scenario("s1", capture_source="hdmi-0")
        mapper.refresh([s])
        src = mapper.resolve_source_for_scenario(s)
        assert src is not None
        assert src.source_type == SourceType.HDMI_CAPTURE
        assert src.capture_source_id == "hdmi-0"

    def test_resolve_source_for_scenario_vnc(self):
        mapper = ScenarioAppMapper()
        s = _scenario("s1", vnc_host="192.168.1.5", vnc_port=5901)
        mapper.refresh([s])
        src = mapper.resolve_source_for_scenario(s)
        assert src is not None
        assert src.source_type == SourceType.VNC
        assert src.vnc_host == "192.168.1.5"
        assert src.vnc_port == 5901

    def test_resolve_source_for_scenario_virtual_desktop(self):
        mapper = ScenarioAppMapper()
        s = _scenario("s1", container_id="ctr-1", wayland_display="wayland-2",
                      width=2560, height=1440, fps=60)
        mapper.refresh([s])
        src = mapper.resolve_source_for_scenario(s)
        assert src is not None
        assert src.source_type == SourceType.VIRTUAL_DESKTOP
        assert src.wayland_display == "wayland-2"
        assert src.width == 2560

    def test_resolve_source_for_scenario_not_in_list(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([])
        src = mapper.resolve_source_for_scenario(_scenario("ghost"))
        assert src is None

    def test_list_apps_full_dict(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([_scenario("s1", capture_source="hdmi-0")])
        apps = mapper.list_apps()
        assert len(apps) == 1
        assert "source_type" in apps[0]
        assert apps[0]["source_type"] == SourceType.HDMI_CAPTURE.value


# ── source_type_counts ────────────────────────────────────────────────────────

class TestSourceTypeCounts:
    def test_counts(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([
            _scenario("a", capture_source="hdmi-0"),
            _scenario("b", capture_source="hdmi-1"),
            _scenario("c", vnc_host="10.0.0.1"),
            _scenario("d"),  # UNKNOWN
        ])
        counts = mapper.source_type_counts()
        assert counts["hdmi_capture"] == 2
        assert counts["vnc"] == 1
        assert counts["unknown"] == 1
        assert counts["virtual_desktop"] == 0
        assert counts["sunshine"] == 0

    def test_empty(self):
        mapper = ScenarioAppMapper()
        mapper.refresh([])
        counts = mapper.source_type_counts()
        assert all(v == 0 for v in counts.values())


# ── on_scenario_change alias ──────────────────────────────────────────────────

class TestOnScenarioChange:
    def test_alias_works(self):
        mapper = ScenarioAppMapper()
        mapper.on_scenario_change([_scenario("s1", capture_source="hdmi-0")])
        assert len(mapper.build_app_list()) == 1


# ── AppSource.to_dict ─────────────────────────────────────────────────────────

class TestAppSourceToDict:
    def test_password_redacted(self):
        src = AppSource(source_type=SourceType.VNC, vnc_password="secret")
        d = src.to_dict()
        assert d["vnc_password"] == "***"

    def test_empty_password_not_redacted(self):
        src = AppSource(source_type=SourceType.VNC, vnc_password="")
        d = src.to_dict()
        assert d["vnc_password"] == ""

    def test_source_type_is_string(self):
        src = AppSource(source_type=SourceType.HDMI_CAPTURE)
        assert src.to_dict()["source_type"] == "hdmi_capture"


# ── MoonlightApp.to_gfe_dict ──────────────────────────────────────────────────

class TestMoonlightAppToGfeDict:
    def test_keys_present(self):
        app = MoonlightApp(id=1, name="Test", scenario_id="s",
                           source_type=SourceType.VNC)
        d = app.to_gfe_dict()
        assert set(d.keys()) == {"ID", "AppTitle", "IsHdrSupported", "HasCustomBoxArt"}

    def test_hdr_int(self):
        app = MoonlightApp(id=1, name="T", scenario_id="s",
                           source_type=SourceType.VNC, hdr_supported=True)
        assert app.to_gfe_dict()["IsHdrSupported"] == 1
