# rye:signed:2026-02-25T08:27:07Z:039ff536780ef616b1643ffa44144719b912a00f22c593f6ca299196830d9637:5_pLupZ9Oby-HisJTuuAxY5jbY1VRlkhvlx3PNz5zVNyM_Vu_N5qb-VZCRJhoeVb2Lh-JH2k6aO7aQdSYsrBCA==:9fbfabe975fa5a7f
__version__ = "1.2.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/adapters"
__tool_description__ = "Tool dispatcher for thread tool calls"

import os
import logging
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import Action
from rye.tools.search import SearchTool
from rye.tools.load import LoadTool
from rye.tools.execute import ExecuteTool
from rye.tools.sign import SignTool
from module_loader import load_module
from rye.utils.resolvers import get_user_space

logger = logging.getLogger(__name__)

_THREADS_ROOT = Path(__file__).resolve().parent.parent


class ToolDispatcher:
    """Dispatch primary tool actions to core RYE tools.

    Translates hook/action dict format to core tool handle() kwargs.

    Action dict format (from hooks and parsed directive actions):
        {"primary": "execute", "item_type": "tool", "item_id": "...", "params": {...}}

    Internal tools (rye/agent/threads/internal/*) are executed in-process
    to preserve live thread context objects (emitter, transcript).

    Core tool handle() kwargs:
        ExecuteTool.handle(item_type=, item_id=, project_path=, parameters=, dry_run=)
        SearchTool.handle(item_type=, query=, project_path=, source=, limit=)
        LoadTool.handle(item_type=, item_id=, project_path=, source=)
        SignTool.handle(item_type=, item_id=, project_path=, source=)
    """

    def __init__(self, project_path: Path):
        self.project_path = project_path
        user_space = str(get_user_space())
        self._tools = {
            Action.EXECUTE: ExecuteTool(user_space),
            Action.SEARCH: SearchTool(user_space),
            Action.LOAD: LoadTool(user_space),
            Action.SIGN: SignTool(user_space),
        }

    def _get(self, action: Dict, params: Dict, key: str, default: Any = "") -> Any:
        """Resolve a key: top-level action attrs first, then params, then default."""
        if key in action:
            return action[key]
        return params.get(key, default)

    async def dispatch(
        self, action: Dict, thread_context: Optional[Dict] = None
    ) -> Dict:
        """Dispatch an action dict to the appropriate core tool."""
        primary = action.get("primary", "execute")
        tool = self._tools.get(primary)
        if not tool:
            return {"status": "error", "error": f"Unknown primary action: {primary}"}

        item_type = action.get("item_type", "tool")
        item_id = action.get("item_id", "")
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
                    item_type=item_type,
                    item_id=item_id,
                    project_path=project_path_str,
                    parameters=params,
                    dry_run=action.get("dry_run", False),
                )
            elif primary == Action.SEARCH:
                return await tool.handle(
                    item_type=item_type,
                    query=self._get(action, params, "query"),
                    project_path=project_path_str,
                    source=self._get(action, params, "source", "project"),
                    limit=self._get(action, params, "limit", 10),
                )
            elif primary == Action.LOAD:
                return await tool.handle(
                    item_type=item_type,
                    item_id=item_id,
                    project_path=project_path_str,
                    source=self._get(action, params, "source", None),
                )
            elif primary == Action.SIGN:
                return await tool.handle(
                    item_type=item_type,
                    item_id=item_id,
                    project_path=project_path_str,
                    source=self._get(action, params, "source", "project"),
                )
        except Exception as e:
            if os.environ.get("RYE_DEBUG"):
                import traceback
                logger.error(
                    "Dispatch %s %s/%s failed: %s\n%s",
                    primary, item_type, item_id, e, traceback.format_exc()
                )
            return {"status": "error", "error": str(e)}

        return {"status": "error", "error": f"Unhandled primary: {primary}"}

    async def dispatch_parallel(
        self, actions: list, thread_context: Optional[Dict] = None
    ) -> list:
        """Dispatch multiple actions concurrently."""
        import asyncio

        tasks = [self.dispatch(action, thread_context) for action in actions]
        return await asyncio.gather(*tasks, return_exceptions=True)
