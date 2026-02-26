"""Bundle entry point for ryeos standard package."""

from pathlib import Path


def get_bundle() -> dict:
    """Return ryeos bundle — standard rye/* items (agent, bash, file-system, mcp, primary, authoring, guides)."""
    return {
        "bundle_id": "ryeos",
        "version": "0.1.2",
        "root_path": Path(__file__).parent,
        "categories": ["rye"],
    }
