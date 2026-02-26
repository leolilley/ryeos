"""Bundle entry point for ryeos-core package."""

from pathlib import Path


def get_bundle() -> dict:
    """Return ryeos-core bundle — rye/core/* items (runtimes, primitives, bundler, extractors)."""
    return {
        "bundle_id": "ryeos-core",
        "version": "0.1.1",
        "root_path": Path(__file__).parent,
        "categories": ["rye/core"],
    }
