"""Bundle entry points for ryeos and ryeos-core.

Both packages ship the same rye Python module but with different .ai/ content.
Each registers its own rye.bundles entry point here.

Other bundle packages (ryeos-web, ryeos-code) have their own entry points
in their respective modules (ryeos_web.bundle, ryeos_code.bundle).
"""

from pathlib import Path


def get_ryeos_bundle() -> dict:
    """Return ryeos bundle — standard rye/* items (excludes web/code, shipped separately)."""
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
