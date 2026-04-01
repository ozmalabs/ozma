"""Root conftest — path setup, markers, and universal fixtures."""

import sys
from pathlib import Path

import pytest

# Fix imports: add all source directories to sys.path so that
# `from agent_engine import AgentEngine` works from any CWD.
_ROOT = Path(__file__).resolve().parent.parent
for _subdir in ("controller", "softnode", "node", "agent"):
    _p = str(_ROOT / _subdir)
    if _p not in sys.path:
        sys.path.insert(0, _p)


def pytest_configure(config):
    config.addinivalue_line("markers", "unit: fast unit test, no external deps")
    config.addinivalue_line("markers", "e2e: end-to-end, requires infrastructure")
    config.addinivalue_line("markers", "security: security/compliance test")
    config.addinivalue_line("markers", "slow: takes >5 seconds")


@pytest.fixture
def data_dir():
    """Path to the test data directory."""
    return Path(__file__).parent / "data"
