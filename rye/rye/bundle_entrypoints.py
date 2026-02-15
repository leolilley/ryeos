"""Bundle entry points for the rye-core package.

Registers: rye/core bundle (only items under rye/core/ categories).
"""

from pathlib import Path


def get_rye_core_bundle() -> dict:
    """Return rye/core bundle — core primitives, runtimes, bundler, extractors."""
    return {
        "bundle_id": "rye/core",
        "version": "0.1.0",
        "root_path": Path(__file__).parent,
        "categories": ["rye/core"],
    }
