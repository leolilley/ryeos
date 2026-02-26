"""Bundle entry point for ryeos-code package."""

from pathlib import Path


def get_bundle() -> dict:
    """Return ryeos-code bundle — rye/code/* items (git, npm, typescript, lsp, diagnostics)."""
    return {
        "bundle_id": "ryeos-code",
        "version": "0.1.2",
        "root_path": Path(__file__).parent,
        "categories": ["rye/code"],
    }
