"""Fetch tool — resolve items by ID or discover by query.

Unifies search and resolution into a single primitive with two modes:
- ID mode (item_id provided): resolve by exact ID, return content + metadata
- Query mode (query + scope provided): discover items by keyword search
"""

import logging
from typing import Any, Dict, Optional, Set

from rye.utils.path_utils import get_user_space
from rye.actions._resolve import resolve_item
from rye.actions._search import search_items

logger = logging.getLogger(__name__)


class FetchTool:
    """Resolve items by ID or discover by query."""

    _ID_PARAMS = {"item_id", "item_type", "source", "destination", "version", "project_path"}
    _QUERY_PARAMS = {"query", "scope", "source", "limit", "project_path"}

    def __init__(self, user_space: Optional[str] = None):
        self.user_space = user_space or str(get_user_space())

    async def handle(self, **kwargs) -> Dict[str, Any]:
        item_id = kwargs.get("item_id")
        query = kwargs.get("query")

        if item_id and query:
            return {"status": "error", "error": "Provide item_id or query, not both"}
        if item_id:
            self._reject_invalid_params(kwargs, self._ID_PARAMS, "ID")
            return await self._resolve_by_id(**kwargs)
        if query:
            self._reject_invalid_params(kwargs, self._QUERY_PARAMS, "query")
            return await self._resolve_by_query(**kwargs)
        return {"status": "error", "error": "Provide item_id or query+scope"}

    def _reject_invalid_params(self, kwargs: Dict, allowed: Set[str], mode_name: str):
        """Fail fast on wrong params for the mode."""
        invalid = set(kwargs.keys()) - allowed
        if invalid:
            raise ValueError(f"Invalid params for {mode_name} mode: {invalid}")

    async def _resolve_by_id(self, **kwargs) -> Dict[str, Any]:
        """ID mode — find item, return content, optionally copy."""
        item_type = kwargs.get("item_type")
        source = kwargs.get("source")

        # Registry requires explicit item_type
        if source == "registry" and not item_type:
            return {"status": "error", "error": "item_type required for registry fetch"}

        if item_type:
            result = await resolve_item(self.user_space, **kwargs)
            if result.get("status") == "success":
                result["mode"] = "id"
            return result

        # Auto-detect: try all types, collect matches
        matches = []
        for try_type in ("directive", "tool", "knowledge"):
            result = await resolve_item(self.user_space, item_type=try_type, **kwargs)
            if result.get("status") == "success":
                matches.append((try_type, result))
            elif result.get("error_type") == "integrity":
                return result

        if len(matches) == 0:
            return {"status": "error", "error": f"Item not found: {kwargs['item_id']}"}
        if len(matches) == 1:
            result = matches[0][1]
            result["item_type"] = matches[0][0]
            result["mode"] = "id"
            return result
        return {
            "status": "error",
            "error": (
                f"Ambiguous item_id '{kwargs['item_id']}' — exists as: "
                f"{', '.join(t for t, _ in matches)}. "
                "Provide item_type to disambiguate."
            ),
        }

    async def _resolve_by_query(self, **kwargs) -> Dict[str, Any]:
        """Query mode — search across spaces."""
        result = await search_items(self.user_space, **kwargs)
        if isinstance(result, dict) and "error" not in result:
            result["mode"] = "query"
        return result
