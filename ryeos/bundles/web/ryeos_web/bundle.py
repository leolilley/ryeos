"""Bundle entry point for ryeos-web package."""

from pathlib import Path


def get_bundle() -> dict:
    """Return ryeos-web bundle — rye/web/* items (browser, fetch, search)."""
    return {
        "bundle_id": "ryeos-web",
        "version": "0.1.1",
        "root_path": Path(__file__).parent,
        "categories": ["rye/web"],
    }
