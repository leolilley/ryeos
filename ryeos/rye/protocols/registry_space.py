"""Registry space provider protocol for extending search and load beyond local spaces.

Registry space providers allow tools (SearchTool, LoadTool) to access items
from external sources (e.g., the Rye Registry) without hardcoding
specific implementations.

Providers are discovered via bundle entry points — bundles declare their
providers in the dict returned by their `rye.bundles` entry point:

    def get_bundle() -> dict:
        return {
            "bundle_id": "ryeos-core",
            ...
            "registry_space_providers": {
                "registry": "rye.core.registry.registry",
            },
        }

The module path is relative to the bundle's .ai/tools/ directory.
The module must export a `get_provider()` function returning a RegistrySpaceProvider.
"""

import logging
from abc import ABC, abstractmethod
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)


class RegistrySpaceProvider(ABC):
    """Interface for remote item spaces (e.g., registry).

    Implementations provide search and pull capabilities for items
    stored outside the local 3-tier space system.
    """

    @property
    @abstractmethod
    def provider_id(self) -> str:
        """Unique identifier for this provider (e.g., 'registry')."""
        ...

    @abstractmethod
    async def search(
        self,
        *,
        query: str,
        item_type: str,
        limit: int = 10,
    ) -> List[Dict[str, Any]]:
        """Search for items in this remote space.

        Args:
            query: Search query string.
            item_type: Filter by type ("directive", "tool", "knowledge").
            limit: Maximum results to return.

        Returns:
            List of result dicts with keys: id, name, description, type,
            source, score, metadata.
        """
        ...

    @abstractmethod
    async def pull(
        self,
        *,
        item_type: str,
        item_id: str,
        version: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Pull (download) an item from this remote space.

        Returns the item content and metadata without writing to disk.
        The caller (LoadTool) handles destination path resolution.

        Args:
            item_type: "directive", "tool", or "knowledge".
            item_id: Item identifier in provider-specific format.
            version: Specific version or None for latest.

        Returns:
            Dict with keys: status, content, item_type, item_id, version,
            metadata (author, namespace, signature, etc.).
            On error: dict with "error" key.
        """
        ...
