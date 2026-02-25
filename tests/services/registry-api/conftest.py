"""Conftest for registry-api tests — add service package to sys.path."""

import sys
from pathlib import Path

_service_root = str(Path(__file__).resolve().parent.parent.parent.parent / "services" / "registry-api")
if _service_root not in sys.path:
    sys.path.insert(0, _service_root)
