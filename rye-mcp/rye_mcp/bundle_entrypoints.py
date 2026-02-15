"""Bundle entry points for the rye-mcp package.

Registers: rye-os bundle (all items under rye/* from rye-core's data).
"""

import importlib.util
from pathlib import Path


def _package_root(pkg: str) -> Path:
    """Locate a package's root path without importing it."""
    spec = importlib.util.find_spec(pkg)
    if not spec or not spec.submodule_search_locations:
        raise RuntimeError(f"Cannot locate package root for {pkg}")
    return Path(next(iter(spec.submodule_search_locations)))


def get_rye_os_bundle() -> dict:
    """Return rye-os bundle — all rye/* items across directives, tools, knowledge."""
    return {
        "bundle_id": "rye-os",
        "version": "0.1.0",
        "root_path": _package_root("rye"),
        "categories": ["rye"],
    }
