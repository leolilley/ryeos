"""Bundle entry point for ryeos-email package."""

from importlib.metadata import version
from pathlib import Path


def get_bundle() -> dict:
    """Return ryeos-email bundle — rye/email/* directives."""
    return {
        "bundle_id": "ryeos-email",
        "version": version("ryeos-email"),
        "root_path": Path(__file__).parent,
        "categories": ["rye/email"],
    }
