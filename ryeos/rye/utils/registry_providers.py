"""Registry space provider discovery and management.

Discovers RegistrySpaceProvider implementations declared by bundles
via the ``rye.bundles`` entry point group.

Bundles declare providers in their entry point dict::

    def get_bundle() -> dict:
        return {
            ...
            "registry_space_providers": {
                "registry": "rye/core/registry/registry",
            },
        }

The value is a slash-separated module path relative to .ai/tools/.
The module must export a ``get_provider()`` function returning a
RegistrySpaceProvider instance.
"""

import importlib.util
import logging
from pathlib import Path
from typing import Dict, Optional

from rye.constants import AI_DIR
from rye.protocols.registry_space import RegistrySpaceProvider
from rye.utils.path_utils import get_system_spaces

logger = logging.getLogger(__name__)

_providers_cache: Optional[Dict[str, RegistrySpaceProvider]] = None


def get_registry_providers() -> Dict[str, RegistrySpaceProvider]:
    """Get all discovered registry space providers, keyed by provider_id.

    Results are cached at module level after first discovery.
    """
    global _providers_cache
    if _providers_cache is not None:
        return _providers_cache

    providers: Dict[str, RegistrySpaceProvider] = {}

    for bundle in get_system_spaces():
        # Check if bundle entry point declared providers
        # Re-load the entry point to get the raw dict with provider declarations
        _discover_bundle_providers(bundle, providers)

    _providers_cache = providers
    return _providers_cache


def get_registry_provider(provider_id: str) -> Optional[RegistrySpaceProvider]:
    """Get a specific registry provider by ID, or None if not found."""
    return get_registry_providers().get(provider_id)


def clear_registry_provider_cache() -> None:
    """Clear the provider cache (useful for testing)."""
    global _providers_cache
    _providers_cache = None


def _discover_bundle_providers(
    bundle, providers: Dict[str, RegistrySpaceProvider]
) -> None:
    """Discover providers declared by a bundle.

    Looks for ``registry_space_providers`` in the bundle's entry point
    dict by re-loading the entry point function.
    """
    import importlib.metadata

    eps = importlib.metadata.entry_points(group="rye.bundles")
    for ep in eps:
        if ep.name != bundle.source:
            continue
        try:
            fn = ep.load()
            result = fn()
            if not isinstance(result, dict):
                continue

            declared = result.get("registry_space_providers")
            if not declared or not isinstance(declared, dict):
                continue

            for provider_id, module_path in declared.items():
                provider = _load_provider_module(
                    bundle.root_path, module_path, provider_id
                )
                if provider:
                    providers[provider.provider_id] = provider

        except Exception:
            logger.debug(
                "Failed to discover providers from bundle %s",
                bundle.bundle_id,
                exc_info=True,
            )


def _load_provider_module(
    bundle_root: Path, module_path: str, provider_id: str
) -> Optional[RegistrySpaceProvider]:
    """Load a provider module from a bundle and call get_provider().

    Args:
        bundle_root: Bundle root directory containing .ai/
        module_path: Slash-separated path relative to .ai/tools/ (no extension)
        provider_id: Expected provider ID (for logging)
    """
    # Convert slash path to filesystem path
    parts = module_path.replace("/", "/")
    file_path = bundle_root / AI_DIR / "tools" / f"{parts}.py"

    if not file_path.exists():
        logger.debug(
            "Provider module not found: %s (expected at %s)",
            module_path, file_path,
        )
        return None

    try:
        spec = importlib.util.spec_from_file_location(
            f"rye_remote_{provider_id}", file_path
        )
        if not spec or not spec.loader:
            return None

        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        # Module must export get_provider()
        get_provider_fn = getattr(module, "get_provider", None)
        if not get_provider_fn or not callable(get_provider_fn):
            logger.debug(
                "Provider module %s missing get_provider() function",
                module_path,
            )
            return None

        provider = get_provider_fn()

        # Duck-type check — provider modules loaded via importlib from
        # bundles can't easily inherit from our ABC, so verify the
        # required interface exists instead.
        required = ("provider_id", "search", "pull")
        missing = [attr for attr in required if not hasattr(provider, attr)]
        if missing:
            logger.debug(
                "get_provider() in %s missing required attributes: %s",
                module_path, missing,
            )
            return None

        return provider

    except Exception:
        logger.debug(
            "Failed to load provider module %s",
            module_path,
            exc_info=True,
        )
        return None
