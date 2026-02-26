"""Bundle entry point for ryeos-code package."""

from importlib.metadata import version
from pathlib import Path


def get_bundle() -> dict:
    """Return ryeos-code bundle — rye/code/* items (git, npm, typescript, lsp, diagnostics)."""
    return {
        "bundle_id": "ryeos-code",
        "version": version("ryeos-code"),
        "root_path": Path(__file__).parent,
        "categories": ["rye/code"],
    }
