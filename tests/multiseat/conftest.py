# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Fixtures for multi-seat tests."""

import sys
from pathlib import Path

import pytest

# Ensure agent/multiseat is importable
_ROOT = Path(__file__).resolve().parent.parent.parent
for _subdir in ("agent", "agent/multiseat", "controller", "softnode"):
    _p = str(_ROOT / _subdir)
    if _p not in sys.path:
        sys.path.insert(0, _p)

pytestmark = pytest.mark.unit
