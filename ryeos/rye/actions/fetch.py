"""Fetch tool — resolve items by ID or discover by query.

Unifies search and resolution into a single primitive with two modes:
- ID mode (item_id provided): resolve by exact ID, return content + metadata
- Query mode (query + scope provided): discover items by keyword search
"""

import logging
from typing import Any, Dict, Optional, Set

from rye.constants import ItemType
from rye.utils.path_utils import get_user_space
from rye.actions._resolve import resolve_item
from rye.actions._search import search_items

logger = logging.getLogger(__name__)


class FetchTool:
    """Resolve items by ID or discover by query."""

    _ID_PARAMS = {"item_id", "item_ref", "source", "destination", "version", "project_path"}
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
        item_id = kwargs.get("item_id", "")

        try:
            ItemType.require_canonical_ref(item_id)
        except ValueError as exc:
            return {"status": "error", "error": str(exc)}

        kwargs = {**kwargs, "item_ref": item_id}
        kwargs.pop("item_id", None)

        result = await resolve_item(self.user_space, **kwargs)
        if result.get("status") == "success":
            result["mode"] = "id"
        return result

    async def _resolve_by_query(self, **kwargs) -> Dict[str, Any]:
        """Query mode — search across spaces."""
        result = await search_items(self.user_space, **kwargs)
        if isinstance(result, dict) and "error" not in result:
            result["mode"] = "query"
        return result
