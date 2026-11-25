#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Compatibility shim for the relocated GStreamer WebRTC worker.

The canonical worker now lives outside the Python node tree at
``scripts/webrtc/gst_webrtc_worker.py`` so the Rust ``ozma-node`` helper path
does not depend on ``softnode/``. This file stays in place for the
one-release Python rollback:

* ``import gst_webrtc_worker`` keeps re-exporting the canonical module's
  symbols (used by ``softnode/gst_webrtc_worker_test.py`` and
  ``softnode/gst_webrtc.py``).
* ``python3 softnode/gst_webrtc_worker.py ...`` keeps working by executing the
  canonical file with the same argv.

Lookup order:

1. ``$OZMA_WEBRTC_WORKER`` (explicit override)
2. ``<repo>/scripts/webrtc/gst_webrtc_worker.py``
3. ``<sys.prefix>/share/ozma/webrtc/gst_webrtc_worker.py``
4. ``$HOME/.local/share/ozma/webrtc/gst_webrtc_worker.py``
5. a legacy in-tree ``_gst_webrtc_worker_impl.py`` if a rollback checkout
   restored one

A pip-installed ``ozma-softnode`` wheel does not ship ``scripts/webrtc/``
today; that packaging gap is documented in ``docs/phase4-install-cutover.md``.
Point ``OZMA_WEBRTC_HELPER_SCRIPT`` (or ``--webrtc-helper-script``) at a
helper installed by ``scripts/install-rust.sh`` instead of relying on the
wheel.
"""

from __future__ import annotations

import importlib.util
import os
import runpy
import sys
from pathlib import Path
from types import ModuleType

_SHIM = Path(__file__).resolve()
_REPO_ROOT = _SHIM.parent.parent

_REMEDIATION = (
    "error: the canonical GStreamer WebRTC worker was not found.\n"
    "Install it with scripts/install-rust.sh, or set OZMA_WEBRTC_WORKER to "
    "the helper path.\n"
    "Expected locations:\n"
    "  <repo>/scripts/webrtc/gst_webrtc_worker.py\n"
    "  <prefix>/share/ozma/webrtc/gst_webrtc_worker.py\n"
)


def _candidates() -> list[Path]:
    paths: list[Path] = []
    override = os.environ.get("OZMA_WEBRTC_WORKER", "").strip()
    if override:
        paths.append(Path(override))
    paths.append(_REPO_ROOT / "scripts" / "webrtc" / "gst_webrtc_worker.py")
    for base in (Path(sys.prefix), Path.home() / ".local"):
        paths.append(base / "share" / "ozma" / "webrtc" / "gst_webrtc_worker.py")
    paths.append(_SHIM.parent / "_gst_webrtc_worker_impl.py")
    return paths


def canonical_path() -> Path | None:
    """Return the first existing canonical worker path, or ``None``."""
    for candidate in _candidates():
        if not candidate.is_file():
            continue
        try:
            if candidate.resolve() == _SHIM:
                continue
        except OSError:
            continue
        return candidate
    return None


def _load_module(path: Path) -> ModuleType:
    spec = importlib.util.spec_from_file_location("_ozma_gst_webrtc_worker", path)
    if spec is None or spec.loader is None:
        raise SystemExit(_REMEDIATION)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    _path = canonical_path()
    if _path is None:
        sys.stderr.write(_REMEDIATION)
        raise SystemExit(2)
    runpy.run_path(str(_path), run_name="__main__")
else:
    _path = canonical_path()
    if _path is None:
        raise SystemExit(_REMEDIATION)
    _module = _load_module(_path)
    for _name, _value in vars(_module).items():
        if _name.startswith("__") or _name in {"__name__"}:
            continue
        globals()[_name] = _value
    if hasattr(_module, "__all__"):
        __all__ = _module.__all__  # type: ignore[assignment]
    del _name, _value
