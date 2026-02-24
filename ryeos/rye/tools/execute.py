"""Execute tool - execute directives, tools, or knowledge items.

Routes execution through PrimitiveExecutor for tools, which handles:
    - Multi-layer routing: Tool → Runtime → Primitive (up to 10 links)
    - On-demand tool loading from .ai/tools/
    - Recursive executor chain resolution via __executor_id__
    - ENV_CONFIG resolution for runtimes
    - Space compatibility validation

Directives support two execution modes controlled by the ``thread`` param:
    - **In-thread** (default, ``thread=False``): Parse, validate inputs,
      interpolate placeholders, and return the directive content with an
      ``instructions`` field for the calling agent to follow in-context.
    - **Threaded** (``thread=True``): Spawn a managed thread via
      ``rye/agent/threads/thread_directive`` (LLM loop, safety harness,
      budgets, registry tracking).  Supports ``async``, ``model``, and
      ``limit_overrides`` sub-parameters.
"""

import logging
import os
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR, DIRECTIVE_INSTRUCTION, ItemType
from rye.directive_parser import parse_and_validate_directive
from rye.executor import ExecutionResult, PrimitiveExecutor
from rye.utils.extensions import get_tool_extensions, get_item_extensions
from rye.utils.parser_router import ParserRouter
from rye.utils.path_utils import (
    get_project_type_path,
    get_system_spaces,
    get_user_type_path,
)
from rye.utils.integrity import verify_item, IntegrityError
from rye.utils.resolvers import get_user_space

logger = logging.getLogger(__name__)

# Re-export for backwards compatibility (used by tests)
from rye.directive_parser import _resolve_input_refs, _interpolate_parsed  # noqa: F401


class ExecuteTool:
    """Execute items (directives, tools, knowledge).

    For tools, uses PrimitiveExecutor for data-driven execution
    with recursive chain resolution.
    """

    def __init__(
        self,
        user_space: Optional[str] = None,
        project_path: Optional[str] = None,
    ):
        """Initialize execute tool.

        Args:
            user_space: User space base path (~ or $USER_SPACE)
            project_path: Project root path for .ai/ resolution
        """
        self.user_space = user_space or str(get_user_space())
        self.project_path = project_path
        self.parser_router = ParserRouter()

        # Lazy-loaded executor (created per-project)
        self._executor: Optional[PrimitiveExecutor] = None

    async def handle(self, **kwargs) -> Dict[str, Any]:
        """Handle execute request."""
        item_type: str = kwargs["item_type"]
        item_id: str = kwargs["item_id"]
        project_path = kwargs["project_path"]
        parameters: Dict[str, Any] = kwargs.get("parameters", {})
        dry_run = kwargs.get("dry_run", False)

        # Thread control params (directives only)
        thread = kwargs.get("thread", False)
        async_exec = kwargs.get("async", False)
        model = kwargs.get("model")
        limit_overrides = kwargs.get("limit_overrides")

        logger.debug(f"Execute: {item_type} item_id={item_id}")

        try:
            start = time.time()

            if item_type == ItemType.DIRECTIVE:
                result = await self._run_directive(
                    item_id, project_path, parameters, dry_run,
                    thread=thread, async_exec=async_exec,
                    model=model, limit_overrides=limit_overrides,
                )
            elif item_type == ItemType.TOOL:
                result = await self._run_tool(
                    item_id, project_path, parameters, dry_run
                )
            elif item_type == ItemType.KNOWLEDGE:
                result = await self._run_knowledge(item_id, project_path)
            else:
                return {
                    "status": "error",
                    "error": f"Unknown item type: {item_type}",
                }

            duration_ms = int((time.time() - start) * 1000)
            if "metadata" not in result:
                result["metadata"] = {}
            result["metadata"]["duration_ms"] = duration_ms

            return result

        except Exception as e:
            logger.error(f"Execute error: {e}")
            return {"status": "error", "error": str(e), "item_id": item_id}

    async def _run_directive(
        self,
        item_id: str,
        project_path: str,
        parameters: Dict[str, Any],
        dry_run: bool,
        *,
        thread: bool = False,
        async_exec: bool = False,
        model: Optional[str] = None,
        limit_overrides: Optional[Dict[str, Any]] = None,
    ) -> Dict[str, Any]:
        """Run a directive — parse, validate, and either return or spawn thread.

        Two modes controlled by ``thread``:

        - **In-thread** (default): Parse, validate inputs, interpolate
          placeholders, and return the directive content with an
          ``instructions`` field.  The calling agent follows the steps
          in its own context.  No LLM infrastructure required.

        - **Threaded** (``thread=True``): After validation, spawn a
          managed thread via ``rye/agent/threads/thread_directive``
          (LLM loop, safety harness, budgets).  Supports ``async``,
          ``model``, and ``limit_overrides``.

        Validation is always done eagerly for fast feedback on bad inputs.
        """
        # 1. Find the directive file
        file_path = self._find_item(project_path, ItemType.DIRECTIVE, item_id)
        if not file_path:
            return {"status": "error", "error": f"Directive not found: {item_id}"}

        # 2. Parse and validate inputs (fast feedback)
        proj_path = Path(project_path) if project_path else None
        validation = parse_and_validate_directive(
            file_path=file_path,
            item_id=item_id,
            parameters=parameters,
            project_path=proj_path,
        )
        if validation["status"] != "success":
            return validation

        # 3. Dry run stops here
        if dry_run:
            return {
                "status": "validation_passed",
                "type": ItemType.DIRECTIVE,
                "item_id": item_id,
                "data": validation["parsed"],
                "inputs": validation["inputs"],
                "message": "Directive validation passed (dry run)",
            }

        # 4a. In-thread mode (default): return lean actionable content
        #     Only what the caller needs to follow the directive:
        #     - instructions: the "go do it" nudge
        #     - body: interpolated process steps (not parsed beyond interpolation)
        #     - outputs: what the directive expects back
        #     No permissions (can't enforce without harness), no parser internals.
        if not thread:
            parsed = validation["parsed"]
            result: Dict[str, Any] = {
                "status": "success",
                "type": ItemType.DIRECTIVE,
                "item_id": item_id,
                "instructions": DIRECTIVE_INSTRUCTION,
                "body": parsed.get("body", ""),
            }
            outputs = parsed.get("outputs")
            if outputs:
                result["outputs"] = outputs
            return result

        # 4b. Threaded mode: spawn managed thread via thread_directive tool
        #     Requires rye/agent infrastructure (thread_directive tool + LLM config)
        td_tool = "rye/agent/threads/thread_directive"
        if not self._find_item(project_path, ItemType.TOOL, td_tool):
            return {
                "status": "error",
                "error": (
                    "thread=true requires the rye/agent thread infrastructure "
                    f"(tool '{td_tool}' not found). "
                    "Either install the rye-agent package or omit thread=true "
                    "to execute the directive in-thread."
                ),
                "item_id": item_id,
            }

        td_params: Dict[str, Any] = {
            "directive_id": item_id,
            "inputs": validation["inputs"],
        }
        if async_exec:
            td_params["async"] = True
        if model:
            td_params["model"] = model
        if limit_overrides:
            td_params["limit_overrides"] = limit_overrides

        # Forward parent thread context if present
        parent_tid = os.environ.get("RYE_PARENT_THREAD_ID")
        if parent_tid:
            td_params["parent_thread_id"] = parent_tid

        thread_result = await self._run_tool(
            "rye/agent/threads/thread_directive",
            project_path,
            td_params,
            dry_run=False,
        )

        # Normalise the response — thread_directive returns its own format
        if thread_result.get("status") == "success" and thread_result.get("data"):
            data = thread_result["data"]
            # Unwrap: PrimitiveExecutor wraps stdout JSON in data.stdout
            if isinstance(data, dict) and "stdout" in data:
                import json as _json
                try:
                    data = _json.loads(data["stdout"])
                except (ValueError, TypeError):
                    data = {"raw": data["stdout"]}
            return {
                "status": "success" if data.get("success", True) else "error",
                "type": ItemType.DIRECTIVE,
                "item_id": item_id,
                **{k: v for k, v in data.items() if k != "success"},
                "metadata": thread_result.get("metadata", {}),
            }

        return thread_result

    async def _run_tool(
        self, item_id: str, project_path: str, parameters: Dict[str, Any], dry_run: bool
    ) -> Dict[str, Any]:
        """Run a tool via PrimitiveExecutor with chain resolution.

        Execution flow:
            1. Get or create PrimitiveExecutor for project
            2. Build executor chain (tool → runtime → primitive)
            3. Validate chain (space compatibility, I/O matching)
            4. Resolve ENV_CONFIG through chain
            5. Execute via root Lilux primitive
        """
        # Get executor for this project
        executor = self._get_executor(project_path)

        if dry_run:
            # Validate chain without executing
            try:
                chain = await executor._build_chain(item_id)
                if not chain:
                    return {"status": "error", "error": f"Tool not found: {item_id}"}

                validation = executor._validate_chain(chain)
                if not validation.valid:
                    return {
                        "status": "error",
                        "error": f"Chain validation failed: {'; '.join(validation.issues)}",
                        "item_id": item_id,
                    }

                return {
                    "status": "validation_passed",
                    "message": "Tool chain validation passed (dry run)",
                    "item_id": item_id,
                    "chain": [executor._chain_element_to_dict(e) for e in chain],
                    "validated_pairs": validation.validated_pairs,
                }
            except Exception as e:
                return {"status": "error", "error": str(e), "item_id": item_id}

        # Execute via PrimitiveExecutor
        result: ExecutionResult = await executor.execute(
            item_id=item_id,
            parameters=parameters,
            validate_chain=True,
        )

        if result.success:
            return {
                "status": "success",
                "type": ItemType.TOOL,
                "item_id": item_id,
                "data": result.data,
                "chain": result.chain,
                "metadata": {
                    "duration_ms": result.duration_ms,
                    **result.metadata,
                },
            }
        else:
            resp = {
                "status": "error",
                "error": result.error,
                "item_id": item_id,
                "chain": result.chain,
                "metadata": {"duration_ms": result.duration_ms},
            }
            if result.data is not None:
                resp["data"] = result.data
            return resp

    def _get_executor(self, project_path: str) -> PrimitiveExecutor:
        """Get or create PrimitiveExecutor for project.

        Creates new executor if project_path changed.
        """
        proj_path = Path(project_path) if project_path else Path.cwd()

        # Check if we need a new executor
        if self._executor is None or self._executor.project_path != proj_path:
            self._executor = PrimitiveExecutor(
                project_path=proj_path,
                user_space=Path(self.user_space),
            )

        return self._executor

    async def _run_knowledge(self, item_id: str, project_path: str) -> Dict[str, Any]:
        """Run/load knowledge - parse and return content."""
        file_path = self._find_item(project_path, ItemType.KNOWLEDGE, item_id)
        if not file_path:
            return {"status": "error", "error": f"Knowledge entry not found: {item_id}"}

        verify_item(file_path, ItemType.KNOWLEDGE, project_path=Path(project_path) if project_path else None)

        content = file_path.read_text(encoding="utf-8")
        parsed = self.parser_router.parse("markdown/frontmatter", content)

        if "name" not in parsed:
            parsed["name"] = item_id

        return {
            "status": "success",
            "type": ItemType.KNOWLEDGE,
            "item_id": item_id,
            "data": parsed,
            "instructions": "Use this knowledge to inform your decisions.",
        }

    def _find_item(
        self, project_path: str, item_type: str, item_id: str
    ) -> Optional[Path]:
        """Find item file by relative path ID searching project > user > system.

        Args:
            item_id: Relative path from .ai/<type>/ without extension.
                    e.g., "rye/core/registry/registry" -> .ai/tools/rye/core/registry/registry.py
        """
        type_dir = ItemType.TYPE_DIRS.get(item_type)
        if not type_dir:
            return None

        # Search order: project > user > system
        # System uses type roots (not category-scoped paths) so item_id
        # resolution matches search — e.g. "rye/core/system" resolves
        # against .ai/directives/ not .ai/directives/rye/core/.
        search_bases = []
        if project_path:
            search_bases.append(get_project_type_path(Path(project_path), item_type))
        search_bases.append(get_user_type_path(item_type))
        type_folder = ItemType.TYPE_DIRS.get(item_type, item_type)
        for bundle in get_system_spaces():
            search_bases.append(bundle.root_path / AI_DIR / type_folder)

        extensions = get_item_extensions(item_type, Path(project_path) if project_path else None)

        for base in search_bases:
            if not base.exists():
                continue
            for ext in extensions:
                file_path = base / f"{item_id}{ext}"
                if file_path.is_file():
                    return file_path

        return None
