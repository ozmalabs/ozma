# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for the ctypes-backed WOFF2 Brotli fallback."""

from __future__ import annotations

import ctypes.util
import importlib
import sys
import types

import pytest

import brotli_fallback as bf

pytestmark = pytest.mark.unit


@pytest.fixture
def woff2_state():
    """Isolate ``sys.modules['brotli']`` and fontTools' WOFF2 flag.

    Tests may install the shim and reload ``fontTools.ttLib.woff2``. Restore
    both the module entries and the WOFF2 module state afterwards so later
    tests see exactly the environment they started with.
    """

    from fontTools.ttLib import woff2

    saved = {
        name: sys.modules.get(name) for name in ("brotli", "brotlicffi")
    }
    # Start each test from a real-environment state, not a previous shim.
    for name, module in saved.items():
        if module is None or getattr(module, "_ozma_ctypes_fallback", False):
            sys.modules.pop(name, None)
    importlib.reload(woff2)
    try:
        yield woff2
    finally:
        for name in ("brotli", "brotlicffi"):
            sys.modules.pop(name, None)
        for name, module in saved.items():
            if module is not None:
                sys.modules[name] = module
        importlib.reload(woff2)


def _require_system_libs(monkeypatch) -> None:
    monkeypatch.setattr(bf, "_python_brotli", lambda: None)
    if bf._load_ctypes_libraries() is None:
        pytest.skip("system libbrotlienc/libbrotlidec are not installed")


@pytest.mark.parametrize(
    "data",
    [
        b"",
        b"The quick brown fox jumps over the lazy dog. " * 32,
        bytes(range(256)) * 4,
        bytes(range(256)) * 1024,  # 256 KiB
    ],
    ids=["empty", "text", "binary", "large"],
)
def test_compress_decompress_round_trip(data, monkeypatch):
    _require_system_libs(monkeypatch)
    compressed = bf.compress(data, mode=bf.MODE_FONT)
    assert bf.decompress(compressed) == data


def test_text_mode_round_trip(monkeypatch):
    _require_system_libs(monkeypatch)
    data = ("Ozma display platform font export. " * 100).encode()
    compressed = bf.compress(data, mode=bf.MODE_TEXT, quality=5)
    assert bf.decompress(compressed) == data


def test_mode_constants_match_libbrotli():
    assert bf.MODE_GENERIC == 0
    assert bf.MODE_TEXT == 1
    assert bf.MODE_FONT == 2


@pytest.mark.parametrize(
    "data",
    [
        b"",
        b"this is not a Brotli stream",
        b"\x06\x00",  # valid empty stream plus trailing garbage
    ],
    ids=["empty-input", "garbage", "trailing"],
)
def test_invalid_data_raises_typed_error(data, monkeypatch):
    _require_system_libs(monkeypatch)
    with pytest.raises(bf.BrotliError):
        bf.decompress(data)
    assert issubclass(bf.BrotliError, ValueError)


def test_truncated_stream_raises_typed_error(monkeypatch):
    _require_system_libs(monkeypatch)
    compressed = bf.compress(b"truncated font data")
    with pytest.raises(bf.BrotliError):
        bf.decompress(compressed[:-1])


def test_invalid_compress_arguments_raise(monkeypatch):
    _require_system_libs(monkeypatch)
    with pytest.raises(ValueError):
        bf.compress(b"data", mode=99)
    with pytest.raises(ValueError):
        bf.compress(b"data", quality=12)
    with pytest.raises(ValueError):
        bf.compress(b"data", lgwin=9)
    with pytest.raises(TypeError):
        bf.compress("not bytes")


def test_available_prefers_python_module(monkeypatch):
    fake = types.SimpleNamespace()
    monkeypatch.setattr(bf, "_python_brotli", lambda: fake)
    monkeypatch.setattr(bf, "_load_ctypes_libraries", lambda: None)
    assert bf.available() is True


def test_available_detects_ctypes_libraries(monkeypatch):
    _require_system_libs(monkeypatch)
    assert bf.available() is True


def test_available_false_when_all_providers_hidden(monkeypatch):
    monkeypatch.setattr(bf, "_python_brotli", lambda: None)
    monkeypatch.setattr(ctypes.util, "find_library", lambda name: None)
    assert bf.available() is False


def test_install_registers_fonttools_compatible_shim(monkeypatch, woff2_state):
    _require_system_libs(monkeypatch)
    sys.modules.pop("brotli", None)
    woff2_state.haveBrotli = False

    assert bf.install() is True
    shim = sys.modules["brotli"]
    assert shim._ozma_ctypes_fallback is True
    assert shim.MODE_GENERIC == bf.MODE_GENERIC
    assert shim.MODE_TEXT == bf.MODE_TEXT
    assert shim.MODE_FONT == bf.MODE_FONT
    assert woff2_state.haveBrotli is True
    assert woff2_state.brotli is shim

    compressed = shim.compress(b"font data", mode=shim.MODE_FONT)
    assert shim.decompress(compressed) == b"font data"


def test_install_fails_without_providers(monkeypatch, woff2_state):
    monkeypatch.setattr(bf, "_python_brotli", lambda: None)
    monkeypatch.setattr(ctypes.util, "find_library", lambda name: None)
    sys.modules.pop("brotli", None)
    woff2_state.haveBrotli = False

    assert bf.install() is False
    assert "brotli" not in sys.modules
    assert woff2_state.haveBrotli is False


def test_ensure_woff2_installs_and_reports(monkeypatch, woff2_state):
    _require_system_libs(monkeypatch)
    sys.modules.pop("brotli", None)
    woff2_state.haveBrotli = False

    with bf.ensure_woff2() as ready:
        assert ready is True
    assert bf.woff2_available() is True
    assert bool(bf.ensure_woff2()) is True
