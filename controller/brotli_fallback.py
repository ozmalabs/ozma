# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Optional Brotli support for WOFF2 without installing a Python package.

``fontTools.ttLib.woff2`` imports a module named ``brotli`` (or
``brotlicffi``) and sets ``haveBrotli`` based on that import. When neither
Python package is installed, this module can load the system
``libbrotlienc``/``libbrotlidec`` shared libraries through :mod:`ctypes` and
register a small module-like shim under ``sys.modules["brotli"]``. Reloading
``fontTools.ttLib.woff2`` afterwards makes WOFF2 reading and writing work
without a network access or a compiler.

The shim is deliberately narrow: it implements the one-shot
``compress``/``decompress`` interface used by fontTools, plus the mode
constants. It never starts a subprocess and never exceeds the configured
input/output bounds. Malformed data raises :class:`BrotliError` (a
``ValueError``) instead of terminating the interpreter.
"""

from __future__ import annotations

import ctypes
import ctypes.util
import importlib
import sys
import threading
import types
from typing import Any

MODE_GENERIC = 0
MODE_TEXT = 1
MODE_FONT = 2

DEFAULT_QUALITY = 11
DEFAULT_LGWIN = 22
MIN_QUALITY = 0
MAX_QUALITY = 11
MIN_LGWIN = 10
MAX_LGWIN = 24

#: Reject inputs larger than this before handing them to libbrotli.
MAX_INPUT_BYTES = 256 * 1024 * 1024
#: Reject compressed outputs larger than this (defensive bound).
MAX_COMPRESSED_BYTES = 256 * 1024 * 1024
#: Reject decompressed outputs larger than this (defensive bound).
MAX_DECOMPRESSED_BYTES = 256 * 1024 * 1024

_DECODE_CHUNK = 64 * 1024
_RESULT_ERROR = 0
_RESULT_SUCCESS = 1
_RESULT_NEEDS_MORE_INPUT = 2
_RESULT_NEEDS_MORE_OUTPUT = 3

_U8P = ctypes.POINTER(ctypes.c_uint8)
_CTYPES_LOCK = threading.Lock()
_INSTALL_LOCK = threading.Lock()
_CTYPES_CACHE: tuple[str, str, tuple[Any, Any]] | None = None

_PYTHON_MODULE_NAMES = ("brotlicffi", "brotli")


class BrotliError(ValueError):
    """Brotli compression or decompression failed."""


def _is_fallback(module: Any) -> bool:
    return getattr(module, "_ozma_ctypes_fallback", False) is True


def _python_brotli() -> Any | None:
    """Return a genuine Python ``brotli``/``brotlicffi`` module, if any."""

    for name in _PYTHON_MODULE_NAMES:
        module = sys.modules.get(name)
        if module is not None and not _is_fallback(module):
            return module
    for name in _PYTHON_MODULE_NAMES:
        try:
            module = importlib.import_module(name)
        except Exception:  # noqa: BLE001 - a broken install must not crash import
            continue
        if not _is_fallback(module):
            return module
    return None


def _configure_libraries(encoder: Any, decoder: Any) -> None:
    encoder.BrotliEncoderMaxCompressedSize.argtypes = [ctypes.c_size_t]
    encoder.BrotliEncoderMaxCompressedSize.restype = ctypes.c_size_t
    encoder.BrotliEncoderCompress.argtypes = [
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_size_t,
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_size_t),
        ctypes.c_void_p,
    ]
    encoder.BrotliEncoderCompress.restype = ctypes.c_int

    decoder.BrotliDecoderCreateInstance.argtypes = [
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_void_p,
    ]
    decoder.BrotliDecoderCreateInstance.restype = ctypes.c_void_p
    decoder.BrotliDecoderDestroyInstance.argtypes = [ctypes.c_void_p]
    decoder.BrotliDecoderDestroyInstance.restype = None
    decoder.BrotliDecoderDecompressStream.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_size_t),
        ctypes.POINTER(_U8P),
        ctypes.POINTER(ctypes.c_size_t),
        ctypes.POINTER(_U8P),
        ctypes.POINTER(ctypes.c_size_t),
    ]
    decoder.BrotliDecoderDecompressStream.restype = ctypes.c_int


def _load_ctypes_libraries() -> tuple[Any, Any] | None:
    """Load and configure the system encoder/decoder, or return ``None``."""

    global _CTYPES_CACHE

    try:
        encoder_path = ctypes.util.find_library("brotlienc")
        decoder_path = ctypes.util.find_library("brotlidec")
    except (OSError, RuntimeError):
        return None
    if not encoder_path or not decoder_path:
        return None

    with _CTYPES_LOCK:
        if _CTYPES_CACHE is not None:
            cached_paths = _CTYPES_CACHE[:2]
            if cached_paths == (encoder_path, decoder_path):
                return _CTYPES_CACHE[2]
        try:
            encoder = ctypes.CDLL(encoder_path)
            decoder = ctypes.CDLL(decoder_path)
            _configure_libraries(encoder, decoder)
        except (OSError, AttributeError):
            return None
        libraries = (encoder, decoder)
        _CTYPES_CACHE = (encoder_path, decoder_path, libraries)
        return libraries


def available() -> bool:
    """Return whether Brotli compression is usable through any provider."""

    if _python_brotli() is not None:
        return True
    return _load_ctypes_libraries() is not None


def _as_bytes(data: Any) -> bytes:
    if isinstance(data, (bytes, bytearray, memoryview)):
        return bytes(data)
    raise TypeError("Brotli data must be bytes-like")


def _check_int(name: str, value: Any, low: int, high: int) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError(f"{name} must be an integer between {low} and {high}")
    if not low <= value <= high:
        raise ValueError(f"{name} must be between {low} and {high}")
    return int(value)


def _check_mode(mode: Any) -> int:
    if mode not in (MODE_GENERIC, MODE_TEXT, MODE_FONT):
        raise ValueError(
            f"mode must be MODE_GENERIC ({MODE_GENERIC}), "
            f"MODE_TEXT ({MODE_TEXT}), or MODE_FONT ({MODE_FONT})"
        )
    return int(mode)


def _ctypes_compress(payload: bytes, mode: int, quality: int, lgwin: int) -> bytes:
    if len(payload) > MAX_INPUT_BYTES:
        raise BrotliError(f"Brotli input exceeds {MAX_INPUT_BYTES} bytes")

    libraries = _load_ctypes_libraries()
    if libraries is None:
        raise BrotliError(
            "system Brotli libraries are unavailable "
            "(install the 'brotli' Python package or libbrotlienc/libbrotlidec)"
        )
    encoder = libraries[0]

    capacity = int(encoder.BrotliEncoderMaxCompressedSize(len(payload)))
    if capacity <= 0:
        capacity = len(payload) + len(payload) // 4 + 1024
    if capacity > MAX_COMPRESSED_BYTES:
        raise BrotliError(f"compressed data would exceed {MAX_COMPRESSED_BYTES} bytes")

    source = (
        ctypes.create_string_buffer(payload)
        if payload
        else ctypes.create_string_buffer(1)
    )
    target = ctypes.create_string_buffer(capacity)
    encoded_size = ctypes.c_size_t(capacity)
    succeeded = encoder.BrotliEncoderCompress(
        quality,
        lgwin,
        mode,
        len(payload),
        ctypes.cast(source, ctypes.c_void_p),
        ctypes.byref(encoded_size),
        ctypes.cast(target, ctypes.c_void_p),
    )
    if not succeeded:
        raise BrotliError("Brotli compression failed")

    size = int(encoded_size.value)
    if size > capacity:
        raise BrotliError("Brotli encoder reported an invalid output size")
    return target.raw[:size]


def _ctypes_decompress(payload: bytes) -> bytes:
    if len(payload) > MAX_COMPRESSED_BYTES:
        raise BrotliError(f"compressed input exceeds {MAX_COMPRESSED_BYTES} bytes")

    libraries = _load_ctypes_libraries()
    if libraries is None:
        raise BrotliError(
            "system Brotli libraries are unavailable "
            "(install the 'brotli' Python package or libbrotlienc/libbrotlidec)"
        )
    decoder = libraries[1]

    source = (
        ctypes.create_string_buffer(payload)
        if payload
        else ctypes.create_string_buffer(1)
    )
    state = decoder.BrotliDecoderCreateInstance(None, None, None)
    if not state:
        raise BrotliError("could not create a Brotli decoder")

    try:
        available_in = ctypes.c_size_t(len(payload))
        next_in = ctypes.cast(source, _U8P)
        chunks: list[bytes] = []
        total = 0
        while True:
            buffer = (ctypes.c_uint8 * _DECODE_CHUNK)()
            available_out = ctypes.c_size_t(_DECODE_CHUNK)
            next_out = ctypes.cast(buffer, _U8P)
            total_out = ctypes.c_size_t(0)
            result = decoder.BrotliDecoderDecompressStream(
                state,
                ctypes.byref(available_in),
                ctypes.byref(next_in),
                ctypes.byref(available_out),
                ctypes.byref(next_out),
                ctypes.byref(total_out),
            )
            produced = _DECODE_CHUNK - int(available_out.value)
            if produced:
                total += produced
                if total > MAX_DECOMPRESSED_BYTES:
                    raise BrotliError(
                        f"decompressed data exceeds {MAX_DECOMPRESSED_BYTES} bytes"
                    )
                chunks.append(ctypes.string_at(buffer, produced))

            if result == _RESULT_SUCCESS:
                if int(available_in.value) != 0:
                    raise BrotliError("Brotli data has trailing bytes")
                return b"".join(chunks)
            if result == _RESULT_NEEDS_MORE_OUTPUT:
                if produced == 0:
                    raise BrotliError("Brotli decoder made no progress")
                continue
            if result == _RESULT_NEEDS_MORE_INPUT:
                raise BrotliError("Brotli data is truncated")
            raise BrotliError("Brotli data is invalid")
    finally:
        decoder.BrotliDecoderDestroyInstance(state)


def compress(
    data: Any,
    mode: int = MODE_GENERIC,
    quality: int = DEFAULT_QUALITY,
    lgwin: int = DEFAULT_LGWIN,
    lgblock: int = 0,
) -> bytes:
    """Compress ``data`` with the same call shape as the ``brotli`` module."""

    payload = _as_bytes(data)
    checked_mode = _check_mode(mode)
    checked_quality = _check_int("quality", quality, MIN_QUALITY, MAX_QUALITY)
    checked_lgwin = _check_int("lgwin", lgwin, MIN_LGWIN, MAX_LGWIN)
    if lgblock:
        # libbrotli's one-shot API chooses its own block size. Accepting the
        # keyword keeps drop-in compatibility without pretending to honour it.
        _check_int("lgblock", lgblock, 0, 24)
    return _ctypes_compress(payload, checked_mode, checked_quality, checked_lgwin)


def decompress(data: Any) -> bytes:
    """Decompress Brotli ``data``, raising :class:`BrotliError` on bad input."""

    return _ctypes_decompress(_as_bytes(data))


def _make_shim() -> types.ModuleType:
    module = types.ModuleType("brotli")
    module.MODE_GENERIC = MODE_GENERIC
    module.MODE_TEXT = MODE_TEXT
    module.MODE_FONT = MODE_FONT
    module.compress = compress
    module.decompress = decompress
    module.error = BrotliError
    module.BrotliError = BrotliError
    module.__version__ = "system-libbrotli"
    module._ozma_ctypes_fallback = True
    return module


def _refresh_woff2() -> bool:
    """Reload ``fontTools.ttLib.woff2`` so its ``haveBrotli`` is current."""

    try:
        import fontTools.ttLib.woff2 as woff2
    except ImportError:
        return False
    try:
        importlib.reload(woff2)
    except Exception:  # noqa: BLE001 - keep plain fontTools usable
        return False
    return bool(getattr(woff2, "haveBrotli", False))


def install() -> bool:
    """Make a Brotli provider importable and refresh fontTools' WOFF2 state.

    A genuine Python ``brotli``/``brotlicffi`` package always wins. Otherwise
    the ctypes shim is registered if the system libraries can be loaded.
    Returns ``True`` when a provider is available, including when fontTools
    itself is not installed.
    """

    with _INSTALL_LOCK:
        return _install()


def _install() -> bool:
    if _python_brotli() is not None:
        _refresh_woff2()
        return True

    if _load_ctypes_libraries() is None:
        stale = sys.modules.get("brotli")
        if _is_fallback(stale):
            # Do not leave fontTools believing WOFF2 works while the shim's
            # backing libraries have disappeared.
            del sys.modules["brotli"]
            _refresh_woff2()
        return False

    existing = sys.modules.get("brotli")
    if not _is_fallback(existing):
        sys.modules["brotli"] = _make_shim()
    _refresh_woff2()
    return True


def woff2_available() -> bool:
    """Return whether fontTools can currently read/write WOFF2."""

    try:
        from fontTools.ttLib import woff2
    except ImportError:
        return False
    if getattr(woff2, "haveBrotli", False):
        return True
    install()
    return bool(getattr(woff2, "haveBrotli", False))


class _EnsureWOFF2:
    """Small truthy context manager returned by :func:`ensure_woff2`."""

    __slots__ = ("available",)

    def __init__(self, available_state: bool) -> None:
        self.available = available_state

    def __bool__(self) -> bool:
        return self.available

    def __enter__(self) -> bool:
        return self.available

    def __exit__(self, exc_type: Any, exc: Any, traceback: Any) -> bool:
        return False


def ensure_woff2() -> _EnsureWOFF2:
    """Install the fallback if needed and report WOFF2 availability.

    Usable directly (``if ensure_woff2(): ...``) or as a context manager
    (``with ensure_woff2() as ready: ...``). Installation is process-wide, so
    exiting the context does not uninstall the shim.
    """

    return _EnsureWOFF2(woff2_available())
