"""Bundle entry points for the rye-os package.

Registers: rye-os bundle (all items under rye/ categories).

The rye-core package uses a separate entry point that registers
only rye/core/ items via get_rye_core_bundle().
"""

from pathlib import Path


def get_rye_os_bundle() -> dict:
    """Return rye-os bundle — all rye/* items across directives, tools, knowledge."""
    return {
        "bundle_id": "rye-os",
        "version": "0.1.0",
        "root_path": Path(__file__).parent,
        "categories": ["rye"],
    }


def get_rye_core_bundle() -> dict:
    """Return rye-core bundle — only rye/core/* items (runtimes, primitives, bundler, extractors)."""
    return {
        "bundle_id": "rye/core",
        "version": "0.1.0",
        "root_path": Path(__file__).parent,
        "categories": ["rye/core"],
    }
