"""Bundle entry points for the ryeos package.

Registers: ryeos bundle (all items under rye/ categories).

The ryeos-core package uses a separate entry point that registers
only rye/core/ items via get_ryeos_core_bundle().

The ryeos-bare package uses get_ryeos_bare_bundle() which registers
no category bundles at all (bare rye, no data-driven tools).
"""

from pathlib import Path


def get_ryeos_bundle() -> dict:
    """Return ryeos bundle — all rye/* items across directives, tools, knowledge."""
    return {
        "bundle_id": "ryeos",
        "version": "0.1.0",
        "root_path": Path(__file__).parent,
        "categories": ["rye"],
    }


def get_ryeos_core_bundle() -> dict:
    """Return ryeos-core bundle — only rye/core/* items (runtimes, primitives, bundler, extractors)."""
    return {
        "bundle_id": "ryeos-core",
        "version": "0.1.0",
        "root_path": Path(__file__).parent,
        "categories": ["rye/core"],
    }


def get_ryeos_bare_bundle() -> dict:
    """Return ryeos-bare bundle — bare rye with no data-driven tools (empty categories)."""
    return {
        "bundle_id": "ryeos-bare",
        "version": "0.1.0",
        "root_path": Path(__file__).parent,
        "categories": [],
    }
