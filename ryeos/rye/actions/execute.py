"""Execute tool — the dumb engine.

The engine resolves an item_id to its ``executor_id`` via the data-driven
extractor system and dispatches.  It does NOT know what directives,
knowledge, or tools are.

Dispatch is determined solely by ``executor_id``:

* ``@primitive_chain`` — the item is self-executing code.  The engine
  dispatches it through ``PrimitiveExecutor`` which handles the
  tool → runtime → primitive chain internally.
* Any other value — the item is data that needs an executor tool.
  The engine calls ``_run_tool(executor_id, {item_id, parameters, …})``.

Allowed thread/target combinations come from the executor tool's metadata
(``__allowed_threads__``, ``__allowed_targets__``), not from a hardcoded
matrix.
"""

import logging
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Literal, Optional

from rye.constants import AI_DIR, ItemType, STATE_DIR, STATE_THREADS
from rye.executor import ExecutionResult, PrimitiveExecutor
from rye.utils.execution_context import ExecutionContext
from rye.utils.extensions import get_item_extensions
from rye.utils.parser_router import ParserRouter
from rye.utils.path_utils import (
    get_project_kind_path,
    get_system_spaces,
    get_user_kind_path,
)
from rye.utils.integrity import verify_item, IntegrityError
from rye.utils.resolvers import get_user_space
from rye.actions._search import get_extraction_rules, get_parser_name

logger = logging.getLogger(__name__)

# Sentinel: item is self-executing via PrimitiveExecutor chain.
PRIMITIVE_CHAIN = "@primitive_chain"


@dataclass(frozen=True)
class ExecutionSpec:
    """Declares what a callee owns in its lifecycle."""

    owner: Literal["engine", "callee"] = "engine"
    native_async: bool = False
    native_resume: bool = False


@dataclass(frozen=True)
class ExecutionPlan:
    """Resolved routing decision for a single execution request."""

    owner: Literal["engine", "callee", "remote"]
    launch_mode: Literal["direct", "engine_detach", "forward_remote"]
    native_async: bool = False
    native_resume: bool = False


@dataclass(frozen=True)
class ResolvedExecutable:
    """Resolved executable artifact — WHERE and HOW to run."""

    kind: str          # "tool", "directive", "knowledge", …
    bare_id: str       # "mytool" or "workflow"
    path: Path
    executor_id: str   # "@primitive_chain" or a tool id

    @property
    def canonical_ref(self) -> str:
        """Canonical item reference (e.g. 'tool:mytool')."""
        return f"{self.kind}:{self.bare_id}"


@dataclass(frozen=True)
class ExecutionEnvelope:
    """WHAT is being executed — the per-request payload.

    Flows through async, resume, and registry without the engine
    ever hardcoding kind-specific prefixes or field names.

    Separate from ExecutionContext (WHERE — project, user space,
    signing keys) which is environment config shared across calls.
    """

    item_ref: str          # canonical ref: "tool:mytool", "directive:workflow"
    executor_id: str       # "@primitive_chain" or executor tool id
    parameters: Dict[str, Any]
    thread: str            # "inline" or "fork"
    async_exec: bool
    dry_run: bool


class ExecuteTool:
    """Execute items by dispatching to their declared executor.

    The engine is kind-agnostic.  Every item declares ``executor_id``
    in its metadata (via the extractor system).  The engine reads it
    and routes accordingly.
    """

    def __init__(
        self,
        user_space: Optional[str] = None,
        project_path: Optional[str] = None,
        extra_env: Optional[Dict[str, str]] = None,
        ctx: Optional[ExecutionContext] = None,
    ):
        if ctx is not None:
            self._base_ctx = ctx
            self.user_space = str(ctx.user_space)
            self.project_path = str(ctx.project_path)
        else:
            self._base_ctx: Optional[ExecutionContext] = None
            self.user_space = user_space or str(get_user_space())
            self.project_path = project_path

        self.parser_router = ParserRouter()
        self.extra_env = extra_env or {}

        # Lazy-loaded executor (created per-project)
        self._executor: Optional[PrimitiveExecutor] = None

    # ------------------------------------------------------------------
    # Resolution
    # ------------------------------------------------------------------

    def _resolve_executable_ref(self, project_path: str, item_id: str) -> ResolvedExecutable:
        """Resolve a canonical ref to a ResolvedExecutable.

        Accepts any kind that has a ``KIND_DIRS`` entry.  Reads
        ``executor_id`` from the extractor system.

        Raises:
            ValueError: If the ref is invalid, bare, the item is not
                found, or the item does not declare an executor.
        """
        kind, bare_id = ItemType.parse_canonical_ref(item_id)

        if not kind:
            raise ValueError(
                f"Canonical ref required for execution "
                f"(e.g. 'tool:{item_id}' or 'directive:{item_id}'). "
                f"Got bare item_id: {item_id!r}"
            )

        if kind not in ItemType.KIND_DIRS:
            raise ValueError(
                f"Unknown kind {kind!r} — no KIND_DIRS entry. Got: {item_id!r}"
            )

        path = self._find_item(project_path, kind, bare_id)
        if not path:
            raise ValueError(f"{kind.capitalize()} not found: {bare_id}")

        # Verify integrity before trusting metadata
        ctx = self._build_ctx(project_path)
        verify_item(path, kind, ctx=ctx)

        # Extract executor_id via the extractor system
        executor_id = self._extract_executor_id(kind, path, project_path)
        if not executor_id:
            raise ValueError(
                f"Item does not declare an executor: {item_id!r}. "
                f"Add executor_id to the item's metadata."
            )

        return ResolvedExecutable(
            kind=kind,
            bare_id=bare_id,
            path=path,
            executor_id=executor_id,
        )

    def _extract_executor_id(
        self, kind: str, path: Path, project_path: str,
    ) -> Optional[str]:
        """Extract executor_id from an item using the extractor system.

        Uses ``get_parser_name(kind)`` and ``get_extraction_rules(kind)``
        to parse the item and apply extraction rules.  Returns the
        ``executor_id`` value or None if not found.
        """
        proj = Path(project_path) if project_path else None
        rules = get_extraction_rules(kind, proj)
        if not rules:
            return None

        executor_rule = rules.get("executor_id")
        if not executor_rule:
            return None

        # Constant rules don't need parsing
        if executor_rule.get("type") == "constant":
            return executor_rule.get("value")

        # Path rules need file content parsed
        parser_name = get_parser_name(kind, proj)
        if not parser_name:
            return None

        try:
            content = path.read_text(encoding="utf-8")
            parsed = self.parser_router.parse(parser_name, content)
            if "error" in parsed:
                return None
            key = executor_rule.get("key", "executor_id")
            return parsed.get(key)
        except Exception:
            return None

    # ------------------------------------------------------------------
    # Main dispatch
    # ------------------------------------------------------------------

    async def handle(self, **kwargs) -> Dict[str, Any]:
        """Handle execute request."""
        item_id: str = kwargs.get("item_id", "")
        project_path = kwargs["project_path"]
        parameters: Dict[str, Any] = kwargs.get("parameters") or {}
        dry_run = kwargs.get("dry_run", False)
        target = kwargs.get("target", "local")
        thread = kwargs.get("thread") or "inline"
        async_exec = kwargs.get("async", False)
        resume_thread_id = kwargs.get("resume_thread_id")

        # Resume mode
        if resume_thread_id:
            return await self._resume(
                resume_thread_id, item_id, project_path, parameters, thread, async_exec,
            )

        if not item_id:
            return {"status": "error", "error": "item_id is required."}

        # Parse target
        try:
            target_mode, remote_name = self._parse_target(target)
        except ValueError as e:
            return {"status": "error", "error": str(e), "item_id": item_id}

        # Resolve executable (includes integrity verification)
        try:
            resolved = self._resolve_executable_ref(project_path, item_id)
        except IntegrityError as e:
            return {
                "status": "error", "error": str(e), "error_type": "integrity", "item_id": item_id,
            }
        except ValueError as e:
            return {"status": "error", "error": str(e), "item_id": item_id}

        # Build envelope — the per-request payload that flows everywhere
        envelope = ExecutionEnvelope(
            item_ref=resolved.canonical_ref,
            executor_id=resolved.executor_id,
            parameters=parameters,
            thread=thread,
            async_exec=async_exec,
            dry_run=dry_run,
        )

        logger.debug(
            f"Execute: {envelope.item_ref} executor={envelope.executor_id} "
            f"target={target} thread={thread} async={async_exec}"
        )

        # Protocol-level validation
        err = self._validate_protocol(target_mode, thread, async_exec, dry_run)
        if err:
            return {"status": "error", "error": err, "item_id": item_id}

        # Executor capability validation
        if envelope.executor_id != PRIMITIVE_CHAIN:
            err = await self._validate_executor_capabilities(
                project_path, envelope.executor_id, target_mode, thread,
            )
            if err:
                return {"status": "error", "error": err, "item_id": item_id}

        try:
            start = time.time()

            if target_mode == "remote":
                result = await self._dispatch_remote(
                    resolved=resolved,
                    envelope=envelope,
                    project_path=project_path,
                    remote_name=remote_name,
                )
            elif envelope.executor_id == PRIMITIVE_CHAIN:
                result = await self._dispatch_primitive_chain(
                    resolved=resolved,
                    envelope=envelope,
                    project_path=project_path,
                )
            else:
                result = await self._dispatch_executor_tool(
                    resolved=resolved,
                    envelope=envelope,
                    project_path=project_path,
                )

            duration_ms = int((time.time() - start) * 1000)
            if "metadata" not in result:
                result["metadata"] = {}
            result["metadata"]["duration_ms"] = duration_ms

            return result

        except IntegrityError as e:
            logger.error(f"Integrity error: {e}")
            return {
                "status": "error", "error": str(e), "error_type": "integrity", "item_id": item_id,
            }
        except Exception as e:
            logger.error(f"Execute error: {e}")
            return {"status": "error", "error": str(e), "item_id": item_id}

    # ------------------------------------------------------------------
    # Validation
    # ------------------------------------------------------------------

    @staticmethod
    def _validate_protocol(
        target: str,
        thread: str,
        async_exec: bool,
        dry_run: bool,
    ) -> Optional[str]:
        """Validate protocol-level constraints (no kind checks).

        Returns an error message string if invalid, None if valid.
        """
        if thread not in ("inline", "fork"):
            return f'Unknown thread mode: {thread!r}. Must be "inline" or "fork".'

        if target not in ("local", "remote"):
            return f'Unknown target: {target!r}. Must be "local", "remote", or "remote:<name>".'

        if async_exec and dry_run:
            return "async + dry_run is not supported. Validation is instant, nothing to detach."

        if dry_run and target == "remote":
            return "dry_run + remote is not supported. Dry run validates locally."

        return None

    async def _validate_executor_capabilities(
        self,
        project_path: str,
        executor_id: str,
        target_mode: str,
        thread: str,
    ) -> Optional[str]:
        """Validate thread/target against executor tool's declared capabilities.

        Returns an error message string if invalid, None if valid.
        """
        caps = await self._read_executor_capabilities(project_path, executor_id)
        allowed_threads = caps.get("allowed_threads", ["inline", "fork"])
        allowed_targets = caps.get("allowed_targets", ["local", "remote"])

        if thread not in allowed_threads:
            return (
                f"Executor tool {executor_id} does not support "
                f"thread={thread!r}. Allowed: {allowed_threads}"
            )
        if target_mode not in allowed_targets:
            return (
                f"Executor tool {executor_id} does not support "
                f"target={target_mode!r}. Allowed: {allowed_targets}"
            )
        return None

    async def _read_executor_capabilities(self, project_path: str, executor_id: str) -> Dict[str, Any]:
        """Read execution capabilities from an executor tool's metadata."""
        defaults = {
            "allowed_threads": ["inline", "fork"],
            "allowed_targets": ["local", "remote"],
        }
        try:
            executor = self._get_executor(project_path)
            chain = await executor._build_chain(executor_id)
            if not chain:
                return defaults
            metadata = executor._load_metadata_cached(chain[0].path)
            return {
                "allowed_threads": metadata.get("allowed_threads", defaults["allowed_threads"]),
                "allowed_targets": metadata.get("allowed_targets", defaults["allowed_targets"]),
            }
        except Exception:
            return defaults

    # ------------------------------------------------------------------
    # Dispatch strategies
    # ------------------------------------------------------------------

    async def _dispatch_primitive_chain(
        self,
        *,
        resolved: ResolvedExecutable,
        envelope: ExecutionEnvelope,
        project_path: str,
    ) -> Dict[str, Any]:
        """Dispatch a self-executing item via PrimitiveExecutor.

        Used when executor_id == "@primitive_chain".
        """
        spec = await self._read_execution_spec(resolved.bare_id, project_path)
        plan = self._resolve_execution_plan(target="local", async_exec=envelope.async_exec, spec=spec)

        if plan.launch_mode == "engine_detach":
            return await self._launch_async(
                envelope=envelope,
                project_path=project_path,
            )

        if envelope.async_exec and not plan.native_async:
            return {
                "status": "error",
                "error": "Item does not support async execution (native_async=False)",
                "item_id": resolved.bare_id,
            }

        parameters = envelope.parameters
        if envelope.async_exec:
            parameters = {**parameters, "async": True}

        return await self._run_tool(resolved.bare_id, project_path, parameters, envelope.dry_run)

    async def _dispatch_executor_tool(
        self,
        *,
        resolved: ResolvedExecutable,
        envelope: ExecutionEnvelope,
        project_path: str,
    ) -> Dict[str, Any]:
        """Dispatch an item to its declared executor tool.

        Used when executor_id is an actual tool (not @primitive_chain).
        The engine passes the envelope to the executor tool.
        """
        if not self._find_item(project_path, ItemType.TOOL, resolved.executor_id):
            return {
                "status": "error",
                "error": (
                    f"Executor tool not found: {resolved.executor_id!r}. "
                    f"Required by {envelope.item_ref}."
                ),
                "item_id": resolved.bare_id,
            }

        tool_params: Dict[str, Any] = {
            "item_id": envelope.item_ref,
            "parameters": envelope.parameters,
            "thread": envelope.thread,
            "async": envelope.async_exec,
            "dry_run": envelope.dry_run,
        }

        result = await self._run_tool(
            resolved.executor_id, project_path, tool_params, dry_run=False,
        )

        # Normalize: preserve the originally requested item identity
        if result.get("status") == "success" and result.get("data"):
            data = result["data"]
            if isinstance(data, dict) and "stdout" in data:
                import json as _json
                try:
                    data = _json.loads(data["stdout"])
                except (ValueError, TypeError):
                    data = {"raw": data["stdout"]}
            return {
                "status": data.get("status", "success"),
                "type": resolved.kind,
                "item_id": resolved.bare_id,
                **{k: v for k, v in data.items() if k not in ("status",)},
                "metadata": result.get("metadata", {}),
            }

        return result

    # ------------------------------------------------------------------
    # Remote dispatch
    # ------------------------------------------------------------------

    async def _dispatch_remote(
        self,
        *,
        resolved: ResolvedExecutable,
        envelope: ExecutionEnvelope,
        project_path: str,
        remote_name: Optional[str],
    ) -> Dict[str, Any]:
        """Dispatch execution to a remote ryeos server.

        Forwards the envelope to the remote tool.
        """
        remote_tool = "rye/core/remote/remote"
        if not self._find_item(project_path, ItemType.TOOL, remote_tool):
            return {
                "status": "error",
                "error": (
                    f"Remote execution requires the remote tool ('{remote_tool}' not found). "
                    'Install ryeos-core or use target="local".'
                ),
                "item_id": resolved.bare_id,
            }

        remote_params = {
            "action": "execute",
            "item_id": envelope.item_ref,
            "parameters": envelope.parameters,
            "thread": envelope.thread,
        }
        if remote_name is not None:
            remote_params["remote"] = remote_name
        if envelope.async_exec:
            remote_params["async"] = True

        remote_result = await self._run_tool(
            remote_tool, project_path, remote_params, dry_run=False,
        )

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
                "type": resolved.kind,
                "item_id": resolved.bare_id,
                "execution_mode": "remote",
                **{k: v for k, v in data.items() if k not in ("status",)},
                "metadata": remote_result.get("metadata", {}),
            }

        return remote_result

    # ------------------------------------------------------------------
    # Resume
    # ------------------------------------------------------------------

    async def _resume(
        self,
        resume_thread_id: str,
        item_id: str,
        project_path: str,
        parameters: Dict[str, Any],
        thread: str,
        async_exec: bool,
    ) -> Dict[str, Any]:
        """Resume a completed or interrupted thread.

        Looks up the thread in the registry, resolves the original item,
        and dispatches through its executor with resume parameters.
        """
        proj = Path(project_path)
        registry = self._get_registry(proj)
        thread_record = registry.get_thread(resume_thread_id) if registry else None

        if not thread_record:
            return {
                "status": "error",
                "error": f"Thread not found: {resume_thread_id}",
                "resume_thread_id": resume_thread_id,
            }

        resolved_id = item_id or thread_record.get("item_id", "")

        if not resolved_id:
            return {
                "status": "error",
                "error": (
                    "Cannot determine item_id for resume: not provided "
                    "and thread record has no item_id field."
                ),
                "resume_thread_id": resume_thread_id,
            }

        try:
            resolved = self._resolve_executable_ref(project_path, resolved_id)
        except IntegrityError as e:
            return {
                "status": "error", "error": str(e), "error_type": "integrity",
                "resume_thread_id": resume_thread_id,
            }
        except ValueError as e:
            return {"status": "error", "error": str(e), "resume_thread_id": resume_thread_id}

        parameters["previous_thread_id"] = resume_thread_id

        envelope = ExecutionEnvelope(
            item_ref=resolved.canonical_ref,
            executor_id=resolved.executor_id,
            parameters=parameters,
            thread=thread if thread != "inline" else "fork",
            async_exec=async_exec,
            dry_run=False,
        )

        if resolved.executor_id == PRIMITIVE_CHAIN:
            spec = await self._read_execution_spec(resolved.bare_id, project_path)
            if not spec.native_resume:
                return {
                    "status": "error",
                    "error": (
                        f"Item {envelope.item_ref} does not support resume "
                        f"(native_resume=False)."
                    ),
                    "resume_thread_id": resume_thread_id,
                }
            parameters["resume"] = True
            parameters["graph_run_id"] = resume_thread_id
            return await self._run_tool(
                resolved.bare_id, project_path, parameters, dry_run=False,
            )
        else:
            return await self._dispatch_executor_tool(
                resolved=resolved,
                envelope=envelope,
                project_path=project_path,
            )

    # ------------------------------------------------------------------
    # Execution spec / plan (for @primitive_chain items)
    # ------------------------------------------------------------------

    async def _read_execution_spec(
        self,
        item_id: str,
        project_path: str,
    ) -> ExecutionSpec:
        """Read execution ownership dunders from the resolved executor chain.

        Builds the chain via PrimitiveExecutor and walks it looking for
        ``execution_owner``.  The first element in the chain that declares
        ownership wins.
        """
        try:
            executor = self._get_executor(project_path)
            chain = await executor._build_chain(item_id)
            if not chain:
                return ExecutionSpec()

            for element in chain:
                metadata = executor._load_metadata_cached(element.path)
                owner = metadata.get("execution_owner")
                if owner is not None:
                    if owner not in ("engine", "callee"):
                        logger.warning(
                            "Item %s (chain element %s) declares unknown "
                            "execution_owner=%r, defaulting to engine",
                            item_id,
                            element.item_id,
                            owner,
                        )
                        owner = "engine"
                    return ExecutionSpec(
                        owner=owner,
                        native_async=bool(metadata.get("native_async", False)),
                        native_resume=bool(metadata.get("native_resume", False)),
                    )

            return ExecutionSpec()
        except Exception:
            logger.warning(
                "Could not read execution spec for %s, defaulting to engine", item_id
            )
            return ExecutionSpec()

    @staticmethod
    def _resolve_execution_plan(
        target: str,
        async_exec: bool,
        spec: ExecutionSpec,
    ) -> ExecutionPlan:
        """Resolve execution routing from spec + request parameters."""
        if target == "remote":
            return ExecutionPlan(owner="remote", launch_mode="forward_remote")

        if spec.owner == "callee":
            return ExecutionPlan(
                owner="callee",
                launch_mode="direct",
                native_async=spec.native_async,
                native_resume=spec.native_resume,
            )

        if async_exec:
            return ExecutionPlan(owner="engine", launch_mode="engine_detach")

        return ExecutionPlan(owner="engine", launch_mode="direct")

    # ------------------------------------------------------------------
    # Target parsing
    # ------------------------------------------------------------------

    @staticmethod
    def _parse_target(target: str) -> tuple:
        """Parse target string into ``(target_mode, remote_name)``."""
        if target.startswith("remote:"):
            name = target[len("remote:"):]
            if not name:
                raise ValueError(
                    'Invalid target "remote:" — remote name cannot be empty. '
                    'Use "remote" for the default remote or "remote:<name>" for a named remote.'
                )
            return ("remote", name)
        if target == "remote":
            return ("remote", None)
        if target == "local":
            return ("local", None)
        raise ValueError(
            f'Unknown target: {target!r}. Must be "local", "remote", or "remote:<name>".'
        )

    # ------------------------------------------------------------------
    # Low-level execution helpers
    # ------------------------------------------------------------------

    async def _run_tool(
        self,
        item_id: str,
        project_path: str,
        parameters: Dict[str, Any],
        dry_run: bool,
    ) -> Dict[str, Any]:
        """Run a tool via PrimitiveExecutor with chain resolution."""
        executor = self._get_executor(project_path)

        if dry_run:
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

        result: ExecutionResult = await executor.execute(
            item_id=item_id,
            parameters=parameters,
            validate_chain=True,
            extra_env=self.extra_env,
        )

        if result.success:
            return {
                "status": "success",
                "type": "tool",
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
        envelope: ExecutionEnvelope,
        project_path: str,
    ) -> Dict[str, Any]:
        """Launch an item in a detached child process, return immediately."""
        import json as _json
        import sys
        from rye.utils.detached import generate_thread_id

        thread_id = generate_thread_id(envelope.item_ref)
        proj = Path(project_path)

        registry = self._get_registry(proj)
        thread_dir = proj / AI_DIR / STATE_DIR / STATE_THREADS / thread_id

        payload = {
            "item_id": envelope.item_ref,
            "parameters": envelope.parameters,
            "target": "local",
            "thread": envelope.thread,
        }

        cmd = [
            sys.executable,
            "-m",
            "rye.utils.async_runner",
            "--project-path",
            project_path,
            "--thread-id",
            thread_id,
        ]

        if registry:
            from rye.utils.detached import spawn_thread

            spawn_result = await spawn_thread(
                registry=registry,
                thread_id=thread_id,
                item_id=envelope.item_ref,
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
                "item_id": envelope.item_ref,
            }

        # Parse kind from canonical ref for response
        kind, bare_id = ItemType.parse_canonical_ref(envelope.item_ref)
        return {
            "status": "success",
            "async": True,
            "thread_id": thread_id,
            "type": kind,
            "item_id": bare_id,
            "execution_mode": envelope.thread,
            "state": "running",
            "pid": spawn_result["pid"],
        }

    @staticmethod
    def _get_registry(project_path: Path):
        """Try to get thread registry. Returns None if unavailable."""
        try:
            from rye.utils.path_utils import get_system_spaces
            from rye.constants import AI_DIR as _AI_DIR

            for bundle in get_system_spaces():
                mod_path = (
                    bundle.root_path
                    / _AI_DIR
                    / "tools"
                    / "rye"
                    / "agent"
                    / "threads"
                    / "persistence"
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

    def _build_ctx(self, project_path: str) -> ExecutionContext:
        """Build an ExecutionContext for the given project path."""
        proj_path = Path(project_path) if project_path else Path.cwd()
        if self._base_ctx is not None:
            return ExecutionContext(
                project_path=proj_path,
                user_space=self._base_ctx.user_space,
                signing_key_dir=self._base_ctx.signing_key_dir,
                system_spaces=self._base_ctx.system_spaces,
            )
        return ExecutionContext.from_env(project_path=proj_path)

    def _get_executor(self, project_path: str) -> PrimitiveExecutor:
        """Get or create PrimitiveExecutor for project."""
        proj_path = Path(project_path) if project_path else Path.cwd()

        if self._executor is None or self._executor.project_path != proj_path:
            ctx = self._build_ctx(project_path)
            self._executor = PrimitiveExecutor(ctx=ctx)

        return self._executor

    def _find_item(
        self, project_path: str, kind: str, item_id: str
    ) -> Optional[Path]:
        """Find item file by relative path ID searching project > user > system."""
        kind_dir = ItemType.KIND_DIRS.get(kind)
        if not kind_dir:
            return None

        search_bases = []
        if project_path:
            search_bases.append(get_project_kind_path(Path(project_path), kind))
        search_bases.append(get_user_kind_path(kind))
        kind_folder = ItemType.KIND_DIRS.get(kind, kind)
        for bundle in get_system_spaces():
            search_bases.append(bundle.root_path / AI_DIR / kind_folder)

        extensions = get_item_extensions(
            kind, Path(project_path) if project_path else None
        )

        for base in search_bases:
            if not base.exists():
                continue
            for ext in extensions:
                file_path = base / f"{item_id}{ext}"
                if file_path.is_file():
                    return file_path

        return None
