"""Execute tool - execute directives, tools, or knowledge items.

Routes execution through PrimitiveExecutor for tools, which handles:
    - Multi-layer routing: Tool → Runtime → Primitive (up to 10 links)
    - On-demand tool loading from .ai/tools/
    - Recursive executor chain resolution via __executor_id__
    - ENV_CONFIG resolution for runtimes
    - Space compatibility validation

Directives support three execution modes controlled by ``parameters.thread``:
    - **Inline** (default, ``thread="inline"``): Parse, validate inputs,
      interpolate placeholders, and return the directive content with an
      ``your_directions`` field for the calling agent to follow in-context.
    - **Fork** (``thread="fork"``): Spawn a managed thread via
      ``rye/agent/threads/thread_directive`` (LLM loop, safety harness,
      budgets, registry tracking).  ``async``, ``model``, and
      ``limit_overrides`` are also passed through parameters.
    - **Remote** (``thread="remote"`` or ``thread="remote:name"``): Push
      execution to a remote ryeos server via ``rye/core/remote/remote``.
      Requires the remote execution tool to be installed.  An optional
      ``:<name>`` suffix selects a named remote (e.g. ``"remote:gpu"``).
"""

import logging
import os
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR, ItemType
from rye.executor import ExecutionResult, PrimitiveExecutor
from rye.utils.extensions import get_tool_extensions, get_item_extensions
from rye.utils.parser_router import ParserRouter
from rye.utils.processor_router import ProcessorRouter
from rye.utils.path_utils import (
    get_project_type_path,
    get_system_spaces,
    get_user_type_path,
)
from rye.utils.integrity import verify_item, IntegrityError
from rye.utils.resolvers import get_user_space

logger = logging.getLogger(__name__)


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
        self.processor_router = ProcessorRouter()

        # Lazy-loaded executor (created per-project)
        self._executor: Optional[PrimitiveExecutor] = None

    async def handle(self, **kwargs) -> Dict[str, Any]:
        """Handle execute request."""
        item_type: str = kwargs["item_type"]
        item_id: str = kwargs["item_id"]
        project_path = kwargs["project_path"]
        parameters: Dict[str, Any] = kwargs.get("parameters", {})
        dry_run = kwargs.get("dry_run", False)
        thread = kwargs.get("thread", "inline")
        async_exec = kwargs.get("async", False)

        logger.debug(f"Execute: {item_type} item_id={item_id}")

        # Step 5 validation: reject invalid async combinations
        if async_exec and dry_run:
            return {
                "status": "error",
                "error": "async + dry_run is not supported. Validation is instant, nothing to detach.",
                "item_id": item_id,
            }
        if async_exec and item_type == ItemType.KNOWLEDGE:
            return {
                "status": "error",
                "error": "async + knowledge is not supported. Knowledge loading is immediate.",
                "item_id": item_id,
            }
        if async_exec and item_type == ItemType.DIRECTIVE and thread == "inline":
            return {
                "status": "error",
                "error": (
                    "async + directive + inline is not supported. "
                    "Inline directives return text for the agent to follow — "
                    "there is nothing to detach. Use thread=\"fork\" for async directives."
                ),
                "item_id": item_id,
            }

        try:
            start = time.time()

            if item_type == ItemType.DIRECTIVE:
                result = await self._run_directive(
                    item_id, project_path, parameters, dry_run,
                    thread=thread, async_exec=async_exec,
                )
            elif item_type == ItemType.TOOL:
                result = await self._run_tool(
                    item_id, project_path, parameters, dry_run,
                    thread=thread, async_exec=async_exec,
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

        except IntegrityError as e:
            logger.error(f"Integrity error: {e}")
            return {"status": "error", "error": str(e), "error_type": "integrity", "item_id": item_id}
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
        thread: str = "inline",
        async_exec: bool = False,
    ) -> Dict[str, Any]:
        """Run a directive — parse, validate, and dispatch to execution mode.

        Three modes controlled by the ``thread`` parameter:

        - **Inline** (default, ``"inline"``): Parse, validate inputs,
          interpolate placeholders, and return the directive content with an
          ``your_directions`` field.  The calling agent follows the steps
          in its own context.  No LLM infrastructure required.

        - **Fork** (``"fork"``): After validation, spawn a managed thread
          via ``rye/agent/threads/thread_directive`` (LLM loop, safety
          harness, budgets).  ``model`` and ``limit_overrides`` are read
          from ``parameters``.

        - **Remote** (``"remote"``): After validation, push execution to
          a remote ryeos server via ``rye/core/remote/remote``.

        Validation is always done eagerly for fast feedback on bad inputs.
        """
        # Extract fork-specific flags from parameters (consumed, not forwarded
        # to directive inputs)
        model = parameters.pop("model", None)
        limit_overrides = parameters.pop("limit_overrides", None)
        # 1. Find the directive file
        file_path = self._find_item(project_path, ItemType.DIRECTIVE, item_id)
        if not file_path:
            return {"status": "error", "error": f"Directive not found: {item_id}"}

        # 2. Integrity check
        proj_path = Path(project_path) if project_path else None
        try:
            verify_item(file_path, ItemType.DIRECTIVE, project_path=proj_path)
        except IntegrityError as exc:
            return {"status": "error", "error": str(exc), "item_id": item_id}

        # 3. Parse
        content = file_path.read_text(encoding="utf-8")
        parsed = self.parser_router.parse("markdown/xml", content)
        if "error" in parsed:
            return {"status": "error", "error": parsed.get("error"), "item_id": item_id}

        # 4. Validate inputs (data-driven processor)
        processor_router = ProcessorRouter(proj_path)
        validation = processor_router.run("inputs/validate", parsed, parameters)
        if validation.get("status") == "error":
            validation["item_id"] = item_id
            return validation

        # 5. Interpolate placeholders (data-driven processor)
        processor_router.run("inputs/interpolate", parsed, validation["inputs"])

        # 6. Dry run stops here
        if dry_run:
            return {
                "status": "validation_passed",
                "type": ItemType.DIRECTIVE,
                "item_id": item_id,
                "data": parsed,
                "inputs": validation["inputs"],
                "message": "Directive validation passed (dry run)",
            }

        # Parse thread mode (supports "remote:name" syntax)
        mode, remote_name = self._parse_thread(thread)

        # 7a. Inline mode (default): return only the directive for
        #     the LLM to follow.  Nothing else — extra fields distract.
        if mode == "inline":
            return {
                "your_directions": parsed.get("body", ""),
            }

        # 7b. Fork mode: spawn managed thread via thread_directive tool
        #     Requires rye/agent infrastructure (thread_directive tool + LLM config)
        if mode == "fork":
            td_tool = "rye/agent/threads/thread_directive"
            if not self._find_item(project_path, ItemType.TOOL, td_tool):
                return {
                    "status": "error",
                    "error": (
                        "thread=\"fork\" requires the rye/agent thread infrastructure "
                        f"(tool '{td_tool}' not found). "
                        "Either install the rye-agent package or use thread=\"inline\" "
                        "to execute the directive inline."
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

        # 7c. Remote mode: push to ryeos-remote, execute there, pull results
        if mode == "remote":
            # Async + remote directive: wrap in detached process
            if async_exec:
                return await self._launch_async(
                    item_type=ItemType.DIRECTIVE,
                    item_id=item_id,
                    project_path=project_path,
                    parameters=validation["inputs"],
                    thread=thread,
                )

            remote_tool = "rye/core/remote/remote"
            if not self._find_item(project_path, ItemType.TOOL, remote_tool):
                return {
                    "status": "error",
                    "error": (
                        "thread=\"remote\" requires the remote execution tool "
                        f"(tool '{remote_tool}' not found). "
                        "Install ryeos-core or use thread=\"inline\" or thread=\"fork\"."
                    ),
                    "item_id": item_id,
                }

            remote_params = {
                "action": "execute",
                "item_type": ItemType.DIRECTIVE,
                "item_id": item_id,
                "parameters": validation["inputs"],
            }
            if remote_name is not None:
                remote_params["remote"] = remote_name

            remote_result = await self._run_tool(
                remote_tool,
                project_path,
                remote_params,
                dry_run=False,
            )

            # Normalise — remote tool returns its own format
            if remote_result.get("status") == "success" and remote_result.get("data"):
                data = remote_result["data"]
                if isinstance(data, dict) and "stdout" in data:
                    import json as _json
                    try:
                        data = _json.loads(data["stdout"])
                    except (ValueError, TypeError):
                        data = {"raw": data["stdout"]}
                return {
                    "status": data.get("status", "success"),
                    "type": ItemType.DIRECTIVE,
                    "item_id": item_id,
                    "execution_mode": "remote",
                    **{k: v for k, v in data.items() if k not in ("status", "success")},
                    "metadata": remote_result.get("metadata", {}),
                }

            return remote_result

        return {
            "status": "error",
            "error": f"Unknown thread mode: '{thread}'. Must be 'inline', 'fork', 'remote', or 'remote:<name>'.",
            "item_id": item_id,
        }

    async def _run_tool(
        self,
        item_id: str,
        project_path: str,
        parameters: Dict[str, Any],
        dry_run: bool,
        *,
        thread: str = "inline",
        async_exec: bool = False,
    ) -> Dict[str, Any]:
        """Run a tool via PrimitiveExecutor with chain resolution.

        Execution modes:
            - **Inline** (default): Execute locally via PrimitiveExecutor.
            - **Remote** (``thread="remote"``): Push execution to a remote
              ryeos server via ``rye/core/remote/remote``.
            - **Fork** not supported for tools (returns error).

        When ``async_exec`` is True for inline/remote tools, execution is
        wrapped in a detached child process via ``launch_detached()`` +
        ``async_runner.py``.  The parent returns immediately with a thread_id.

        Execution flow (inline):
            1. Get or create PrimitiveExecutor for project
            2. Build executor chain (tool → runtime → primitive)
            3. Validate chain (space compatibility, I/O matching)
            4. Resolve ENV_CONFIG through chain
            5. Execute via root Lillux primitive
        """
        # Parse thread mode (supports "remote:name" syntax)
        mode, remote_name = self._parse_thread(thread)

        if mode == "fork":
            return {
                "status": "error",
                "error": (
                    'thread="fork" is not supported for tools. '
                    "Fork spawns a managed LLM thread, which only applies to directives. "
                    'Use thread="inline" (default) or thread="remote".'
                ),
                "item_id": item_id,
            }

        # Async tool execution: spawn detached process, return immediately
        if async_exec:
            return await self._launch_async(
                item_type=ItemType.TOOL,
                item_id=item_id,
                project_path=project_path,
                parameters=parameters,
                thread=thread,
            )

        # Remote mode: push to ryeos-remote server
        if mode == "remote":
            remote_tool = "rye/core/remote/remote"
            if not self._find_item(project_path, ItemType.TOOL, remote_tool):
                return {
                    "status": "error",
                    "error": (
                        'thread="remote" requires the remote execution tool '
                        f"(tool '{remote_tool}' not found). "
                        'Install ryeos-core or use thread="inline".'
                    ),
                    "item_id": item_id,
                }

            remote_params = {
                "action": "execute",
                "item_type": ItemType.TOOL,
                "item_id": item_id,
                "parameters": parameters,
            }
            if remote_name is not None:
                remote_params["remote"] = remote_name

            remote_result = await self._run_tool(
                remote_tool, project_path, remote_params, dry_run=False,
            )

            # Normalise — remote tool returns its own format
            if remote_result.get("status") == "success" and remote_result.get("data"):
                data = remote_result["data"]
                if isinstance(data, dict) and "stdout" in data:
                    import json as _json
                    try:
                        data = _json.loads(data["stdout"])
                    except (ValueError, TypeError):
                        data = {"raw": data["stdout"]}
                return {
                    "status": data.get("status", "success"),
                    "type": ItemType.TOOL,
                    "item_id": item_id,
                    "execution_mode": "remote",
                    **{k: v for k, v in data.items() if k not in ("status", "success")},
                    "metadata": remote_result.get("metadata", {}),
                }

            return remote_result

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

    async def _launch_async(
        self,
        *,
        item_type: str,
        item_id: str,
        project_path: str,
        parameters: Dict[str, Any],
        thread: str = "inline",
    ) -> Dict[str, Any]:
        """Launch execution in a detached child process, return immediately.

        Generates a uuid-based thread_id, registers in the ThreadRegistry,
        spawns ``async_runner.py`` via ``launch_detached()``, and returns
        a handle dict with thread_id + pid.

        Follows the same pattern as thread_directive.py's async spawn path:
        thread_id registered in ThreadRegistry, log dir at
        ``.ai/agent/threads/{thread_id}/``, results stored via
        ``registry.set_result()``.
        """
        import json as _json
        import sys
        import uuid as _uuid

        thread_id = str(_uuid.uuid4())
        proj = Path(project_path)

        registry = self._get_registry(proj)
        thread_dir = proj / ".ai" / "agent" / "threads" / thread_id

        payload = {
            "item_type": item_type,
            "item_id": item_id,
            "parameters": parameters,
            "thread": thread,
        }

        cmd = [
            sys.executable, "-m", "rye.utils.async_runner",
            "--project-path", project_path,
            "--thread-id", thread_id,
        ]

        if registry:
            from rye.utils.detached import spawn_thread
            spawn_result = await spawn_thread(
                registry=registry,
                thread_id=thread_id,
                directive=f"{item_type}/{item_id}",
                cmd=cmd,
                log_dir=thread_dir,
                input_data=_json.dumps(payload),
            )
        else:
            from rye.utils.detached import launch_detached
            spawn_result = await launch_detached(
                cmd,
                thread_id=thread_id,
                log_dir=thread_dir,
                input_data=_json.dumps(payload),
            )

        if not spawn_result.get("success"):
            return {
                "status": "error",
                "error": f"Failed to spawn async process: {spawn_result.get('error')}",
                "item_id": item_id,
            }

        return {
            "status": "success",
            "async": True,
            "thread_id": thread_id,
            "type": item_type,
            "item_id": item_id,
            "execution_mode": thread,
            "state": "running",
            "pid": spawn_result["pid"],
        }

    @staticmethod
    def _parse_thread(thread: str) -> tuple:
        """Parse thread string into ``(mode, remote_name)``.

        Returns:
            A 2-tuple ``(mode, remote_name)`` where *mode* is one of
            ``"inline"``, ``"fork"``, or ``"remote"``, and *remote_name*
            is the named remote suffix (``None`` when unspecified).

        Examples::

            "inline"       -> ("inline", None)
            "fork"         -> ("fork", None)
            "remote"       -> ("remote", None)
            "remote:gpu"   -> ("remote", "gpu")
        """
        if thread.startswith("remote:"):
            return ("remote", thread[len("remote:"):])
        return (thread, None)

    @staticmethod
    def _get_registry(project_path: Path):
        """Try to get thread registry. Returns None if unavailable."""
        try:
            from rye.utils.path_utils import get_system_spaces
            from rye.constants import AI_DIR as _AI_DIR

            for bundle in get_system_spaces():
                mod_path = (
                    bundle.root_path / _AI_DIR / "tools"
                    / "rye" / "agent" / "threads" / "persistence"
                    / "thread_registry.py"
                )
                if mod_path.is_file():
                    import importlib.util
                    spec = importlib.util.spec_from_file_location(
                        "thread_registry", mod_path
                    )
                    mod = importlib.util.module_from_spec(spec)
                    spec.loader.exec_module(mod)
                    return mod.get_registry(project_path)
        except Exception:
            pass
        return None

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
            "your_directions": "Use this knowledge to inform your decisions.",
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
