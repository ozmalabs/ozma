# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Bounded, deterministic fuzz regression for the display importers.

WGT-082: malformed sources must produce a typed error or a lossless capture;
they must never panic, hang, or allocate without bound.  This file is a normal
pytest module (no libFuzzer/AFL dependency).  Every case is seeded, sized, and
wrapped in a wall-clock alarm so a regression shows up as a reproducible test
failure rather than a stuck CI job.

Run with::

    python -m pytest tests/unit/controller/test_display_fuzz.py -q
"""

from __future__ import annotations

import hashlib
import io
import math
import signal
import struct
import time
import zipfile
from contextlib import contextmanager
from typing import Iterator

import pytest

from display_importers.adaptive_cards import (
    MAX_SOURCE_BYTES as ADAPTIVE_MAX_SOURCE_BYTES,
)
from display_importers.adaptive_cards import AdaptiveCardsImporter
from display_importers.aida64 import MAX_SOURCE_BYTES as AIDA_MAX_SOURCE_BYTES
from display_importers.aida64 import Aida64Importer
from display_importers.base import ImportContext, ImportError
from display_importers.grafana import MAX_SOURCE_BYTES as GRAFANA_MAX_SOURCE_BYTES
from display_importers.grafana import GrafanaImporter
from display_importers.home_assistant import MAX_SOURCE_BYTES as HA_MAX_SOURCE_BYTES
from display_importers.home_assistant import HomeAssistantImporter
from display_importers.magicmirror import (
    MAX_DEPTH as MAGICMIRROR_MAX_DEPTH,
)
from display_importers.magicmirror import (
    MAX_SOURCE_BYTES as MAGICMIRROR_MAX_SOURCE_BYTES,
)
from display_importers.magicmirror import MagicMirrorImporter
from display_importers.rainmeter import MAX_SOURCE_BYTES as RAINMETER_MAX_SOURCE_BYTES
from display_importers.rainmeter import RainmeterImporter
from font_translation import MAX_SOURCE_BYTES as FONT_MAX_SOURCE_BYTES
from font_translation import FontError, FontMetadata
from shape_extract import (
    MAX_TEXT_INPUT as SHAPE_MAX_TEXT_INPUT,
)
from shape_extract import (
    ShapeExtractError,
    extract_from_css,
    extract_from_svg,
)

pytestmark = pytest.mark.unit

#: Per-call wall-clock budget.  A hostile input that exceeds this is a hang.
CASE_SECONDS = 2.0
#: Keep fuzz inputs small so the whole module stays well under ~10 seconds.
FUZZ_INPUT_BYTES = 24 * 1024
#: Main thumbnail cap for fuzzed output.  Bounded output is part of the test.
MAX_WIDGETS = 50_000


# ---------------------------------------------------------------------------
# Deterministic RNG and bounded execution helpers
# ---------------------------------------------------------------------------


class _Rng:
    """Tiny deterministic LCG so failures are reproducible from a seed."""

    def __init__(self, seed: int) -> None:
        self.state = seed & 0xFFFFFFFF

    def u32(self) -> int:
        self.state = (1664525 * self.state + 1013904223) & 0xFFFFFFFF
        return self.state

    def below(self, limit: int) -> int:
        return self.u32() % limit

    def bytes(self, length: int) -> bytes:
        return bytes(self.below(256) for _ in range(length))


class _CaseTimeout(TimeoutError):
    """Raised by the per-case alarm; deliberately not an ``ImportError``."""


def _alarm(_signum: int, _frame: object) -> None:
    raise _CaseTimeout(f"case exceeded {CASE_SECONDS:.1f}s")


@contextmanager
def _bounded(seconds: float = CASE_SECONDS) -> Iterator[None]:
    """Fail instead of hanging when a parser loops or allocates forever."""
    if not hasattr(signal, "SIGALRM"):  # pragma: no cover - non-POSIX only
        yield
        return
    previous = signal.getsignal(signal.SIGALRM)
    signal.signal(signal.SIGALRM, _alarm)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0.0)
        signal.signal(signal.SIGALRM, previous)


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _assert_bounded_import(name: str, importer: object, data: bytes):
    """Import ``data`` and enforce the lossless/bounded contract."""
    started = time.monotonic()
    try:
        with _bounded():
            result = importer.import_source(  # type: ignore[attr-defined]
                name, data, ImportContext(name=name)
            )
    except ImportError:
        # A typed rejection is a valid outcome; nothing is written anywhere.
        return None
    elapsed = time.monotonic() - started
    assert elapsed < CASE_SECONDS, f"{name}: import took {elapsed:.2f}s"
    assert result.lossless is True, f"{name}: capture was not lossless"
    assert result.source_capture["source_hash"] == _sha256(data)
    assert result.raw == data, f"{name}: raw bytes were not retained"
    assert len(result.widgets) <= MAX_WIDGETS, f"{name}: unbounded widget output"
    return result


def _mutate(rng: _Rng, base: bytes, *, max_length: int = FUZZ_INPUT_BYTES) -> bytes:
    """Return a deterministic malformed/truncated/oversized copy of ``base``."""
    payload = bytearray(base)
    operation = rng.below(7)
    if operation == 0 and payload:  # truncate
        payload = payload[: rng.below(len(payload) + 1)]
    elif operation == 1 and payload:  # random byte flips
        for _ in range(1 + rng.below(8)):
            payload[rng.below(len(payload))] = rng.below(256)
    elif operation == 2:  # random insertion
        at = rng.below(len(payload) + 1)
        payload[at:at] = rng.bytes(1 + rng.below(64))
    elif operation == 3 and payload:  # duplicate a slice (expansion)
        # Cap before inserting so a mutation cannot itself allocate unboundedly.
        room = max(0, max_length - len(payload))
        take = min(len(payload), room, rng.below(256) + 1)
        payload.extend(payload[:take])
    elif operation == 4:  # all random bytes
        payload = bytearray(rng.bytes(1 + rng.below(max_length)))
    elif operation == 5:  # control characters and NULs
        at = rng.below(len(payload) + 1)
        payload[at:at] = b"\x00\xff\xfe\x1b\r\n"
    else:  # byte reversal
        payload.reverse()
    return bytes(payload[:max_length])


# ---------------------------------------------------------------------------
# Archive helpers (built in memory; never touch the filesystem)
# ---------------------------------------------------------------------------


def _zip_bytes(entries: list[tuple[str, bytes]]) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in entries:
            archive.writestr(name, data)
    return buffer.getvalue()


def _zip_with_symlink(name: str = "link.xml", target: str = "../secret") -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        info = zipfile.ZipInfo(name)
        info.create_system = 3
        info.external_attr = (0o120777 << 16) | 0o777
        archive.writestr(info, target.encode())
    return buffer.getvalue()


def _patch_zip_headers(
    data: bytes,
    *,
    set_flag: int | None = None,
    uncompressed_size: int | None = None,
    local_header_offset: int | None = None,
) -> bytes:
    """Patch local + central headers to simulate hostile metadata."""
    payload = bytearray(data)
    for signature, flag_offset, size_offset in (
        (b"PK\x03\x04", 6, 22),  # local file header
        (b"PK\x01\x02", 8, 24),  # central directory header
    ):
        cursor = 0
        while True:
            cursor = payload.find(signature, cursor)
            if cursor < 0:
                break
            if set_flag is not None:
                payload[cursor + flag_offset] |= set_flag
            if uncompressed_size is not None:
                payload[cursor + size_offset : cursor + size_offset + 4] = (
                    struct.pack("<I", uncompressed_size)
                )
            if local_header_offset is not None and signature == b"PK\x01\x02":
                payload[cursor + 42 : cursor + 46] = struct.pack(
                    "<I", local_header_offset
                )
            cursor += 4
    return bytes(payload)


def _compression_bomb() -> bytes:
    """~4 MiB of zeros compressed to a few KiB (ratio > 1000)."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(
        buffer, "w", zipfile.ZIP_DEFLATED, compresslevel=9
    ) as archive:
        archive.writestr("panel.xml", b"\x00" * (4 * 1024 * 1024))
    return buffer.getvalue()


# ---------------------------------------------------------------------------
# Generic importer fuzzing
# ---------------------------------------------------------------------------

_IMPORTER_CASES = [
    (
        "aida64",
        Aida64Importer(),
        "panel.rslcd",
        [
            b'<sensorpanel><gauge name="cpu" value="12"/></sensorpanel>',
            b'{"panels":[{"type":"gauge","value":1}]}',
            b'<html><body><span class="Simple1">x</span></body></html>',
            _zip_bytes([("panel.xml", b"<sensorpanel/>")]),
        ],
    ),
    (
        "rainmeter",
        RainmeterImporter(),
        "skin.ini",
        [
            b"[Rainmeter]\nUpdate=1000\n[MeterCpu]\nMeter=Bar\nMeasureName=MeasureCpu\n",
            b"[MeasureCpu]\nMeasure=CPU\nMaxValue=100\n",
        ],
    ),
    (
        "home_assistant",
        HomeAssistantImporter(),
        "dashboard.yaml",
        [
            b"views:\n  - cards:\n      - type: gauge\n        entity: sensor.cpu\n",
            b"type: entities\nentities:\n  - sensor.cpu\n",
        ],
    ),
    (
        "magicmirror",
        MagicMirrorImporter(),
        "config.js",
        [
            b"var config = { modules: [ { module: 'clock', position: 'top_left' } ] };",
            b"let config = { modules: [] };",
        ],
    ),
    (
        "grafana",
        GrafanaImporter(),
        "dashboard.json",
        [
            b'{"dashboard":{"panels":[{"type":"gauge","title":"CPU"}]}}',
            b'{"type":"stat","title":"mem","targets":[]}',
        ],
    ),
    (
        "adaptive_cards",
        AdaptiveCardsImporter(),
        "card.json",
        [
            b'{"type":"AdaptiveCard","version":"1.5","body":['
            b'{"type":"TextBlock","text":"hi"}]}',
        ],
    ),
]


@pytest.mark.parametrize(
    ("label", "importer", "name", "seeds"),
    _IMPORTER_CASES,
    ids=[case[0] for case in _IMPORTER_CASES],
)
def test_importer_fuzz_is_typed_or_lossless(
    label: str, importer: object, name: str, seeds: list[bytes]
) -> None:
    """Seeded malformed/truncated/oversized inputs stay bounded."""
    rng = _Rng(0x57544708 ^ sum(ord(char) for char in label))
    for seed in seeds:
        for _ in range(12):
            _assert_bounded_import(name, importer, _mutate(rng, seed))

    # Pure random bytes exercise the sniffer/classifier and opaque capture.
    for _ in range(8):
        _assert_bounded_import(name, importer, rng.bytes(1 + rng.below(4096)))

    # Oversized-but-bounded input (fuzz self-cap, not a production limit claim).
    oversized = rng.bytes(FUZZ_INPUT_BYTES)
    _assert_bounded_import(name, importer, oversized)


def test_importer_fuzz_is_deterministic() -> None:
    """The same seeded input produces the same capture hash and widget count."""
    importer = GrafanaImporter()
    data = b'{"dashboard":{"panels":[{"type":"gauge","title":"CPU"}]}}'
    first = _assert_bounded_import("dashboard.json", importer, data)
    second = _assert_bounded_import("dashboard.json", importer, data)
    assert first is not None and second is not None
    assert first.source_capture["source_hash"] == second.source_capture[
        "source_hash"
    ]
    assert len(first.widgets) == len(second.widgets)


# ---------------------------------------------------------------------------
# Archive attack cases (AIDA64 ZIP container)
# ---------------------------------------------------------------------------

_ARCHIVE_REJECTED = {
    "path_traversal": _zip_bytes([("../evil.xml", b"<sensorpanel/>")]),
    "path_traversal_backslash": _zip_bytes(
        [("..\\..\\evil.xml", b"<sensorpanel/>")]
    ),
    "absolute_posix": _zip_bytes([("/etc/passwd", b"root")]),
    "absolute_windows": _zip_bytes([("C:\\Windows\\evil.xml", b"<x/>")]),
    "symlink": _zip_with_symlink(),
    "encrypted": _patch_zip_headers(
        _zip_bytes([("panel.xml", b"<sensorpanel/>")]), set_flag=0x1
    ),
    "size_mismatch": _patch_zip_headers(
        _zip_bytes([("panel.xml", b"<sensorpanel/>")]),
        uncompressed_size=0xFFFFFFF0,
    ),
    # Fuzz-found: zipfile raises BadZipFile/ValueError from archive.open() and
    # the importer does not translate it to ImportError.
    "corrupt_local_header_offset": _patch_zip_headers(
        _zip_bytes([("panel.xml", b"<sensorpanel/>")]),
        local_header_offset=0xFFFF0000,
    ),
    "compression_bomb": _compression_bomb(),
}


@pytest.mark.parametrize("case", sorted(_ARCHIVE_REJECTED))
def test_archive_cases_fail_closed(case: str) -> None:
    data = _ARCHIVE_REJECTED[case]
    with _bounded():
        with pytest.raises(ImportError):
            Aida64Importer().import_source(
                "panel.rslcd", data, ImportContext(name="panel.rslcd")
            )


def test_archive_duplicate_names_stay_bounded() -> None:
    data = _zip_bytes(
        [("panel.xml", b"<sensorpanel/>"), ("panel.xml", b"<sensorpanel/>")]
    )
    result = _assert_bounded_import("panel.rslcd", Aida64Importer(), data)
    assert result is not None
    assert result.lossless
    assert [asset.path for asset in result.assets].count("panel.xml") == 2


def test_archive_too_many_members_rejected() -> None:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_STORED) as archive:
        for index in range(4097):
            archive.writestr(f"m{index}.txt", b"")
    data = buffer.getvalue()
    with _bounded():
        with pytest.raises(ImportError, match="limit"):
            Aida64Importer().import_source(
                "panel.rslcd", data, ImportContext(name="panel.rslcd")
            )


def test_archive_truncated_central_directory_is_bounded() -> None:
    data = _zip_bytes([("panel.xml", b"<sensorpanel/>")])
    truncated = data[: len(data) // 2]
    # A truncated archive may be classified as opaque payload instead of a ZIP;
    # either way it must not raise an untyped exception or hang.
    _assert_bounded_import("panel.rslcd", Aida64Importer(), truncated)


# ---------------------------------------------------------------------------
# YAML cases (Home Assistant)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "payload",
    [
        b"!!python/object/apply:os.system ['echo pwned']\n",
        b"a: !!python/object/new:os.system []\n",
        b"a: !!python/name:os.system\n",
    ],
)
def test_yaml_unsafe_tags_rejected(payload: bytes) -> None:
    with _bounded():
        with pytest.raises(ImportError):
            HomeAssistantImporter().import_source(
                "dashboard.yaml", payload, ImportContext(name="dashboard.yaml")
            )


def test_yaml_alias_expansion_stays_bounded() -> None:
    lines = ['a0: &a0 ["x", "x"]']
    for level in range(1, 40):
        lines.append(f'a{level}: &a{level} [*a{level - 1}, *a{level - 1}]')
    lines.append("type: entities")
    lines.append("entities: [sensor.cpu]")
    payload = ("\n".join(lines) + "\n").encode()
    result = _assert_bounded_import(
        "dashboard.yaml", HomeAssistantImporter(), payload
    )
    assert result is not None and result.lossless


def test_yaml_deep_nesting_is_bounded() -> None:
    payload = ("views: " + "[" * 2000 + "0" + "]" * 2000 + "\n").encode()
    with _bounded():
        with pytest.raises(ImportError):
            HomeAssistantImporter().import_source(
                "dashboard.yaml", payload, ImportContext(name="dashboard.yaml")
            )


def test_yaml_huge_scalar_is_bounded() -> None:
    payload = (
        b"type: markdown\ncontent: \""
        + b"x" * 200_000
        + b"\"\n"
    )
    result = _assert_bounded_import(
        "dashboard.yaml", HomeAssistantImporter(), payload
    )
    assert result is not None and result.lossless


# ---------------------------------------------------------------------------
# MagicMirror JavaScript cases (never executed)
# ---------------------------------------------------------------------------


def test_magicmirror_braces_in_strings_and_comments() -> None:
    payload = (
        b"// config = { fake: true }\n"
        b"var config = {\n"
        b"  /* a comment with } and { */\n"
        b"  modules: [ { module: 'clock', config: { label: '}', expr: '{x}' } } ]\n"
        b"};\n"
    )
    result = _assert_bounded_import("config.js", MagicMirrorImporter(), payload)
    assert result is not None and result.lossless
    assert len(result.widgets) == 1
    assert result.extensions["magicmirror"]["executed"] is False


@pytest.mark.parametrize(
    "payload",
    [
        b"var config = { modules: [ /* unterminated",
        b"var config = { modules: [ { module: 'clock', config: { x: require('fs') } } ] };",
        b"var config = { modules: [ { module: 'clock', config: { x: f() } } ] };",
        b"var config = { modules: [ { module: 'clock', config: `a${b}` } ] };",
        b"var config = { modules: " + b"[" * 200 + b"]" * 200 + b" };",
    ],
)
def test_magicmirror_unparseable_js_is_opaque_and_not_executed(
    payload: bytes,
) -> None:
    result = _assert_bounded_import("config.js", MagicMirrorImporter(), payload)
    assert result is not None and result.lossless
    assert result.widgets == []
    extension = result.extensions["magicmirror"]
    assert extension["executed"] is False
    assert extension["opaque_source"] == payload.decode("utf-8", "replace")


def test_magicmirror_source_size_limit_is_enforced_before_parse() -> None:
    payload = b"var config = { modules: [] };" + b" " * (
        MAGICMIRROR_MAX_SOURCE_BYTES + 1
    )
    result = _assert_bounded_import("config.js", MagicMirrorImporter(), payload)
    assert result is not None and result.lossless
    assert "size limit" in result.extensions["magicmirror"]["parse_error"]
    assert result.extensions["magicmirror"]["limits"]["max_source_bytes"] == (
        MAGICMIRROR_MAX_SOURCE_BYTES
    )
    assert MAGICMIRROR_MAX_DEPTH <= 128


# ---------------------------------------------------------------------------
# Font metadata parsing
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("seed", range(10))
def test_font_random_bytes_raise_typed_error(seed: int) -> None:
    rng = _Rng(0xF017 ^ seed)
    data = rng.bytes(1 + rng.below(512))
    with _bounded():
        with pytest.raises(FontError):
            FontMetadata.from_bytes(data)


def test_font_malformed_sfnt_headers_raise_typed_error() -> None:
    cases: list[tuple[str, bytes]] = [
        (f"truncated-{length}", b"\x00\x01\x00\x00" + b"\x00" * length)
        for length in (0, 1, 4, 8, 16, 32, 64)
    ]
    cases += [
        ("wrong-version-otto", b"OTTO" + b"\x00" * 20),
        ("wrong-version-true", b"true" + b"\x00" * 20),
        ("wrong-version-ttcf", b"ttcf" + b"\x00" * 20),
    ]
    untyped: list[str] = []
    for label, data in cases:
        with _bounded():
            try:
                FontMetadata.from_bytes(data)
            except FontError:
                continue
            except Exception as exc:  # noqa: BLE001 - report the leaked type
                untyped.append(f"{label}: {type(exc).__name__}: {exc}")
            else:
                untyped.append(f"{label}: returned metadata instead of an error")
    assert not untyped, "font errors were not typed: " + "; ".join(untyped)


def test_font_absurd_table_length_raises_typed_error() -> None:
    # sfnt header: version, numTables=1, search fields, then one table record
    # named `head` with an offset past EOF and a 4 GiB declared length.
    data = (
        b"\x00\x01\x00\x00"
        + struct.pack(">HHHH", 1, 0, 0, 0)
        + struct.pack(">4sIII", b"head", 0, 12, 0xFFFFFFFF)
    )
    with _bounded():
        with pytest.raises(FontError):
            FontMetadata.from_bytes(data)


# ---------------------------------------------------------------------------
# Shape extraction (SVG/CSS)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("seed", range(24))
def test_shape_random_bytes_are_typed_or_bounded(seed: int) -> None:
    rng = _Rng(0x5A9E ^ seed)
    alphabet = b"<>/{}\"'=:;-_abcdefghijklmnopqrstuvwxyz0123456789 \n\t&"
    text = "".join(
        chr(alphabet[rng.below(len(alphabet))]) for _ in range(rng.below(4096))
    )
    for function in (extract_from_svg, extract_from_css):
        started = time.monotonic()
        try:
            with _bounded():
                output = function(text)
        except ShapeExtractError:
            continue
        assert time.monotonic() - started < CASE_SECONDS
        assert len(output["colors"]) <= max(1, len(text))
        assert len(output["gradients"]) <= max(1, len(text))


def test_shape_entity_references_rejected() -> None:
    payload = '<svg viewBox="0 0 10 10"><!ENTITY x "y"></svg>'
    with _bounded():
        with pytest.raises(ShapeExtractError):
            extract_from_svg(payload)


@pytest.mark.parametrize(
    "payload",
    [
        "<svg><!-- unterminated",
        "<svg",
        "<svg viewBox='0 0 10 10'><g/></svg><!-- tail",
    ],
)
def test_shape_unterminated_constructs_are_typed(payload: str) -> None:
    with _bounded():
        with pytest.raises(ShapeExtractError):
            extract_from_svg(payload)


def test_shape_trailing_junk_is_bounded() -> None:
    """Trailing junk may be ignored, but it must stay bounded and typed."""
    payload = "<svg viewBox='0 0 10 10'></svg><"
    try:
        with _bounded():
            output = extract_from_svg(payload)
    except ShapeExtractError:
        return
    assert output["shape"]["aspect_ratio"] == 1.0


def test_shape_deep_nesting_is_bounded() -> None:
    payload = '<svg viewBox="0 0 10 10">' + "<g>" * 3000 + "</g>" * 3000 + "</svg>"
    with _bounded():
        output = extract_from_svg(payload)
    assert output["shape"]["aspect_ratio"] == 1.0


def test_shape_huge_gradients_are_bounded() -> None:
    declaration = (
        "background: linear-gradient(90deg, #112233 0%, #445566 100%);"
    )
    payload = ".a{" + declaration * 4000 + "}"
    assert len(payload.encode()) <= SHAPE_MAX_TEXT_INPUT
    with _bounded():
        output = extract_from_css(payload)
    assert output["gradients"] == ["linear-gradient"]
    assert output["colors"] == ["#112233", "#445566"]


def test_shape_oversized_input_rejected_before_parse() -> None:
    payload = "<svg viewBox='0 0 10 10'>" + "x" * SHAPE_MAX_TEXT_INPUT + "</svg>"
    with _bounded():
        with pytest.raises(ShapeExtractError):
            extract_from_svg(payload)


def test_shape_css_unbalanced_and_unterminated_are_typed() -> None:
    for payload in (".a{", ".a{color:red}}", ".a{content:'unterminated}", ".a{/*"):
        with _bounded():
            with pytest.raises(ShapeExtractError):
                extract_from_css(payload)


# ---------------------------------------------------------------------------
# Limit enforcement (a missing production limit must fail this test)
# ---------------------------------------------------------------------------


def test_source_size_limits_are_declared_and_enforced() -> None:
    """Every expensive parser must cap input before doing work.

    These importers currently declare no source-level byte cap.  The API layer
    has a 32 MiB request cap, but direct controller/import-store callers reach
    the parser without that backstop, so this test intentionally fails and
    names the missing limits instead of silently skipping them.
    """
    # Every direct parser must declare and enforce a source cap before work.
    importers = [
        ("aida64", Aida64Importer(), "panel.xml", AIDA_MAX_SOURCE_BYTES),
        ("rainmeter", RainmeterImporter(), "skin.ini", RAINMETER_MAX_SOURCE_BYTES),
        ("home_assistant", HomeAssistantImporter(), "dashboard.yaml", HA_MAX_SOURCE_BYTES),
        ("grafana", GrafanaImporter(), "dashboard.json", GRAFANA_MAX_SOURCE_BYTES),
    ]
    for label, importer, name, limit in importers:
        assert limit > 0, label
        with _bounded():
            with pytest.raises(ImportError):
                importer.import_source(
                    name,
                    b"x" * (limit + 1),
                    ImportContext(name=name),
                )

    assert FONT_MAX_SOURCE_BYTES > 0
    with _bounded():
        with pytest.raises(FontError):
            FontMetadata.from_bytes(b"\x00" * (FONT_MAX_SOURCE_BYTES + 1))

    # Limits that already existed must also be enforced before the parse.
    assert ADAPTIVE_MAX_SOURCE_BYTES > 0
    with _bounded():
        with pytest.raises(ImportError):
            AdaptiveCardsImporter().import_source(
                "card.json",
                b"{" + b" " * ADAPTIVE_MAX_SOURCE_BYTES + b"}",
                ImportContext(name="card.json"),
            )

    assert SHAPE_MAX_TEXT_INPUT > 0
    with _bounded():
        with pytest.raises(ShapeExtractError):
            extract_from_css("a" * (SHAPE_MAX_TEXT_INPUT + 1))


def test_fuzz_bounds_are_sane() -> None:
    """Guard against accidentally turning the fuzz suite into a slow test."""
    assert CASE_SECONDS <= 3.0
    assert 0 < FUZZ_INPUT_BYTES <= 64 * 1024
    assert math.isfinite(CASE_SECONDS)
