"""Bundle entry point for ryeos-web package."""

from importlib.metadata import version
from pathlib import Path


def get_bundle() -> dict:
    """Return ryeos-web bundle — rye/web/* items (browser, fetch, search)."""
    return {
        "bundle_id": "ryeos-web",
        "version": version("ryeos-web"),
        "root_path": Path(__file__).parent,
        "categories": ["rye/web"],
    }
