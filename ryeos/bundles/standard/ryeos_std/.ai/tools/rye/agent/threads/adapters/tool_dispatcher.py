# rye:signed:2026-04-20T05:46:18Z:3369423f0a11e7b2577896a74457409fc467ef0f5c0e75fd75beb94ea78c7b59:LaWVWGAsyM0RN-IUx1h2-377W4i3z7kLQq17cyMsAxcVgpEm6owFMZZfzUWAyzi-vHvUqpfPClDzfaTbC-u4AA:4b987fd4e40303ac
__version__ = "1.2.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/adapters"
__tool_description__ = "Tool dispatcher for thread tool calls"

import logging
import os
from pathlib import Path
from typing import Any, Dict

from rye.actions.execute import ExecuteTool
from rye.actions.fetch import FetchTool
from rye.actions.sign import SignTool
from rye.constants import Action, ItemType
from rye.utils.resolvers import get_user_space

logger = logging.getLogger(__name__)


class ToolDispatcher:
    """Dispatch primary actions to core RYE tools.

    Translates hook/action dict format to core tool handle() kwargs.

    Action dict format (from hooks and parsed directive actions):
        {"primary": "execute", "kind": "tool", "item_id": "...", "params": {...}}

    Internal tools (rye/agent/threads/internal/*) are executed in-process
    to preserve live thread context objects (emitter, transcript).

    Core tool handle() kwargs:
        ExecuteTool.handle(item_id=, project_path=, parameters=, dry_run=)
        FetchTool.handle(item_id=, project_path=, source=, query=, scope=, limit=)
        SignTool.handle(item_id=, project_path=, source=)
    """

    def __init__(self, project_path: Path):
        self.project_path = project_path
        user_space = str(get_user_space())
        self._tools = {
            Action.EXECUTE: ExecuteTool(user_space),
            Action.FETCH: FetchTool(user_space),
            Action.SIGN: SignTool(user_space),
        }

    def _get(self, action: Dict, params: Dict, key: str, default: Any = "") -> Any:
        """Resolve a key: top-level action attrs first, then params, then default."""
        if key in action:
            return action[key]
        return params.get(key, default)

    async def dispatch(self, action: Dict) -> Dict:
        """Dispatch an action dict to the appropriate core tool."""
        primary = action.get("primary", "execute")
        tool = self._tools.get(primary)
        if not tool:
            return {"status": "error", "error": f"Unknown primary action: {primary}"}

        kind = action.get("kind", "tool")
        bare_id = action.get("item_id", "")
        item_ref = ItemType.make_canonical_ref(kind, bare_id) if bare_id else ""
        params = dict(action.get("params", {}))

        project_path_str = str(self.project_path)

        # LLMs sometimes pass `parameters` as a JSON string instead of an
        # object.  Detect and parse so downstream tools receive a dict.
        if "parameters" in params and isinstance(params["parameters"], str):
            import json

            try:
                params["parameters"] = json.loads(params["parameters"])
            except (json.JSONDecodeError, ValueError):
                pass  # leave as-is; tool will report the real error

        try:
            if primary == Action.EXECUTE:
                return await tool.handle(
                    item_id=item_ref,
                    project_path=project_path_str,
                    parameters=params,
                    dry_run=action.get("dry_run", False),
                )
            elif primary == Action.FETCH:
                # Pass through all fetch params — FetchTool handles mode detection
                fetch_kwargs = {"project_path": project_path_str}
                # ID mode params — pass canonical ref
                if bare_id:
                    fetch_kwargs["item_id"] = item_ref
                # Query mode params
                query = self._get(action, params, "query", None)
                if query:
                    fetch_kwargs["query"] = query
                scope = self._get(action, params, "scope", None)
                if scope:
                    fetch_kwargs["scope"] = scope
                # Shared params
                source = self._get(action, params, "source", None)
                if source:
                    fetch_kwargs["source"] = source
                destination = self._get(action, params, "destination", None)
                if destination:
                    fetch_kwargs["destination"] = destination
                limit = self._get(action, params, "limit", None)
                if limit:
                    fetch_kwargs["limit"] = limit
                return await tool.handle(**fetch_kwargs)
            elif primary == Action.SIGN:
                return await tool.handle(
                    item_id=item_ref,
                    project_path=project_path_str,
                    source=self._get(action, params, "source", "project"),
                )
        except Exception as e:
            if os.environ.get("RYE_DEBUG"):
                import traceback

                logger.error(
                    "Dispatch %s %s/%s failed: %s\n%s",
                    primary,
                    kind,
                    bare_id,
                    e,
                    traceback.format_exc(),
                )
            return {"status": "error", "error": str(e)}

        return {"status": "error", "error": f"Unhandled primary: {primary}"}

    async def dispatch_parallel(self, actions: list) -> list:
        """Dispatch multiple actions concurrently."""
        import asyncio

        tasks = [self.dispatch(action) for action in actions]
        return await asyncio.gather(*tasks, return_exceptions=True)
