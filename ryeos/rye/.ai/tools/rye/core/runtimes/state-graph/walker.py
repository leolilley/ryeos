# rye:signed:2026-02-23T04:44:21Z:4ad7f6fd5b52c8826ac231db6f2a44042f7b16b4a3f51c8392396ba67bb5d056:j-El4Pbb20eFhYZgFQ9uYu4g5q6mhkFeK4KBPkriH6jluXoYAdj-Jih-M6RxEwb3_bQ09IRBIV8MLl-kfYKwAg==:9fbfabe975fa5a7f
"""
state_graph_walker.py: Graph traversal engine for state graph tools.

Walks a graph YAML tool definition, dispatching rye_execute calls for each
node action.  State is persisted as a signed knowledge item after each step.
Graph runs register in the thread registry for status tracking and
wait_threads support.

Entry point: same pattern as thread_directive.py — argparse + asyncio.run().
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/runtimes/state-graph"
__tool_description__ = "State graph walker — traverses graph YAML tools"

import argparse
import asyncio
import fnmatch
import json
import logging
import os
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

import yaml

from rye.constants import AI_DIR, ItemType
from rye.utils.metadata_manager import MetadataManager
from rye.utils.resolvers import get_user_space
from rye.tools.execute import ExecuteTool
from rye.tools.search import SearchTool
from rye.tools.load import LoadTool
from rye.tools.sign import SignTool

from module_loader import load_module

logger = logging.getLogger(__name__)

_ANCHOR = Path(__file__).resolve().parent.parent.parent / "agent" / "threads"


# ---------------------------------------------------------------------------
# Core tool handles (same instances as ToolDispatcher uses)
# ---------------------------------------------------------------------------

_user_space = None


def _get_user_space() -> str:
    global _user_space
    if _user_space is None:
        _user_space = str(get_user_space())
    return _user_space


def _get_tools():
    us = _get_user_space()
    return {
        "execute": ExecuteTool(us),
        "search": SearchTool(us),
        "load": LoadTool(us),
        "sign": SignTool(us),
    }


_tools: Optional[Dict] = None


def _tools_instance():
    global _tools
    if _tools is None:
        _tools = _get_tools()
    return _tools


# ---------------------------------------------------------------------------
# Graph loading
# ---------------------------------------------------------------------------


def _load_graph_yaml(graph_path: str) -> Dict:
    """Load and parse a graph tool YAML file."""
    path = Path(graph_path)
    if not path.exists():
        raise FileNotFoundError(f"Graph tool not found: {graph_path}")

    content = path.read_text(encoding="utf-8")
    # Strip rye signature lines before parsing YAML
    lines = content.split("\n")
    clean = [l for l in lines if not l.strip().startswith("# rye:signed:")]
    return yaml.safe_load("\n".join(clean))


# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------


async def _dispatch_action(action: Dict, project_path: str) -> Dict:
    """Dispatch a node action through the appropriate primary tool.

    Same action dict format as ToolDispatcher.dispatch().  All core tool
    handles are async — we await them directly.
    """
    tools = _tools_instance()
    primary = action.get("primary", "execute")
    item_type = action.get("item_type", "tool")
    item_id = action.get("item_id", "")
    params = action.get("params", {})

    try:
        if primary == "execute":
            return await tools["execute"].handle(
                item_type=item_type,
                item_id=item_id,
                project_path=project_path,
                parameters=params,
            )
        elif primary == "search":
            return await tools["search"].handle(
                item_type=item_type,
                query=params.get("query", ""),
                project_path=project_path,
                source=params.get("source", "project"),
                limit=params.get("limit", 10),
            )
        elif primary == "load":
            return await tools["load"].handle(
                item_type=item_type,
                item_id=item_id,
                project_path=project_path,
                source=params.get("source", "project"),
            )
        elif primary == "sign":
            return await tools["sign"].handle(
                item_type=item_type,
                item_id=item_id,
                project_path=project_path,
                source=params.get("source", "project"),
            )
        else:
            return {"status": "error", "error": f"Unknown primary: {primary}"}
    except Exception as e:
        if os.environ.get("RYE_DEBUG"):
            import traceback
            logger.error("Dispatch %s %s/%s failed: %s\n%s",
                         primary, item_type, item_id, e, traceback.format_exc())
        return {"status": "error", "error": str(e)}


# ---------------------------------------------------------------------------
# Result unwrapping (same logic as runner.py._clean_tool_result)
# ---------------------------------------------------------------------------

_DROP_KEYS = frozenset(("chain", "metadata", "path", "source", "resolved_env_keys"))


def _unwrap_result(raw_result: Any) -> Dict:
    """Unwrap rye_execute envelope to get the inner tool result.

    The ExecuteTool returns ``{status, type, item_id, data, chain, metadata}``.
    The actual tool output lives in ``data``.  We always lift ``data`` to the
    top level so graph ``assign`` expressions like ``${result.stdout}`` work
    naturally.

    Error propagation: if the outer envelope has ``status: "error"`` or the
    inner data has ``success: false``, the unwrapped result will have
    ``status: "error"`` so the graph walker's error handling (on_error edges,
    hooks, error_mode) fires correctly.
    """
    if not isinstance(raw_result, dict):
        return {"result": raw_result} if raw_result is not None else {}

    outer_error = raw_result.get("status") == "error"

    inner = raw_result.get("data")
    if isinstance(inner, dict):
        unwrapped = {k: v for k, v in inner.items() if k not in _DROP_KEYS}
        if outer_error or unwrapped.get("success") is False:
            unwrapped["status"] = "error"
            if outer_error and "error" in raw_result:
                unwrapped.setdefault("error", raw_result["error"])
        return unwrapped

    unwrapped = {k: v for k, v in raw_result.items() if k not in _DROP_KEYS}
    if outer_error or unwrapped.get("success") is False:
        unwrapped["status"] = "error"
    return unwrapped


# ---------------------------------------------------------------------------
# Execution context and permissions
# ---------------------------------------------------------------------------


def _read_thread_meta(project_path: str, thread_id: str) -> Optional[Dict]:
    """Read a thread's thread.json."""
    meta_path = Path(project_path) / AI_DIR / "threads" / thread_id / "thread.json"
    if meta_path.exists():
        with open(meta_path, "r", encoding="utf-8") as f:
            return json.load(f)
    return None


def _resolve_execution_context(
    params: Dict, project_path: str, graph_config: Optional[Dict] = None,
) -> Dict:
    """Resolve capabilities and parent context for permission enforcement.

    Resolution order:
    1. Parent thread env var (inherited from spawning thread)
    2. Explicit capabilities in params (programmatic callers)
    3. Graph YAML permissions field (declared by the graph author)
    4. Fail-closed (no capabilities)
    """
    parent_thread_id = os.environ.get("RYE_PARENT_THREAD_ID")

    if parent_thread_id:
        meta = _read_thread_meta(project_path, parent_thread_id)
        if meta:
            transcript_signer = load_module(
                "persistence/transcript_signer", anchor=_ANCHOR
            )
            if not transcript_signer.verify_json(meta):
                logger.warning(
                    "thread.json signature invalid for %s — fail-closed",
                    parent_thread_id,
                )
                return {
                    "parent_thread_id": None,
                    "capabilities": [],
                    "limits": {},
                    "depth": 0,
                }
            return {
                "parent_thread_id": parent_thread_id,
                "capabilities": meta.get("capabilities", []),
                "limits": meta.get("limits", {}),
                "depth": meta.get("limits", {}).get("depth", 0),
            }

    if "capabilities" in params:
        return {
            "parent_thread_id": None,
            "capabilities": params["capabilities"],
            "limits": params.get("limits", {}),
            "depth": params.get("depth", 5),
        }

    # Graph-declared permissions: the graph YAML itself declares what it needs
    if graph_config:
        graph_caps = graph_config.get("permissions", [])
        if graph_caps:
            return {
                "parent_thread_id": None,
                "capabilities": graph_caps,
                "limits": graph_config.get("limits", {}),
                "depth": graph_config.get("limits", {}).get("depth", 5),
            }

    # No thread context, no explicit capabilities — fail-closed
    return {
        "parent_thread_id": None,
        "capabilities": [],
        "limits": {},
        "depth": 0,
    }


def _check_permission(
    exec_ctx: Dict, primary: str, item_type: str, item_id: str
) -> Optional[Dict]:
    """Check if action is permitted by resolved capabilities.

    Same logic as SafetyHarness.check_permission():
    - Empty capabilities = deny all (fail-closed)
    - Internal thread tools always allowed
    - fnmatch wildcards for glob matching
    """
    if item_id and item_id.startswith("rye/agent/threads/internal/"):
        return None

    capabilities = exec_ctx.get("capabilities", [])
    if not capabilities:
        return {
            "status": "error",
            "error": (
                f"Permission denied: no capabilities. "
                f"Cannot {primary} {item_type} '{item_id}'"
            ),
        }

    if item_id:
        item_id_dotted = item_id.replace("/", ".")
        required = f"rye.{primary}.{item_type}.{item_id_dotted}"
    else:
        required = f"rye.{primary}.{item_type}"

    for cap in capabilities:
        if fnmatch.fnmatch(required, cap):
            return None

    return {
        "status": "error",
        "error": (
            f"Permission denied: '{required}' not covered by capabilities"
        ),
    }


# ---------------------------------------------------------------------------
# Parent context injection for LLM thread spawns
# ---------------------------------------------------------------------------


def _inject_parent_context(params: Dict, exec_ctx: Dict) -> Dict:
    """Inject parent thread context for child thread spawns."""
    params = dict(params)
    if exec_ctx.get("parent_thread_id"):
        params.setdefault("parent_thread_id", exec_ctx["parent_thread_id"])
    if exec_ctx.get("depth") is not None:
        params.setdefault("parent_depth", exec_ctx["depth"])
    if exec_ctx.get("limits"):
        params.setdefault("parent_limits", exec_ctx["limits"])
    if exec_ctx.get("capabilities"):
        params.setdefault("parent_capabilities", exec_ctx["capabilities"])
    return params


# ---------------------------------------------------------------------------
# Hooks
# ---------------------------------------------------------------------------


def _merge_graph_hooks(
    graph_hooks: List[Dict], project_path: str
) -> List[Dict]:
    """Merge graph-level hooks with applicable builtins.

    Same pattern as thread_directive._merge_hooks().
    Filters out inapplicable thread-only events.
    """
    hooks_loader = load_module("loaders/hooks_loader", anchor=_ANCHOR)
    loader = hooks_loader.get_hooks_loader()
    proj = Path(project_path)
    builtin = loader.get_builtin_hooks(proj)
    infra = loader.get_infra_hooks(proj)

    EXCLUDED_EVENTS = {"context_limit_reached", "thread_started"}
    builtin = [h for h in builtin if h.get("event") not in EXCLUDED_EVENTS]
    infra = [h for h in infra if h.get("event") not in EXCLUDED_EVENTS]

    for h in graph_hooks:
        h.setdefault("layer", 1)
    for h in builtin:
        h.setdefault("layer", 2)
    for h in infra:
        h.setdefault("layer", 3)

    return sorted(
        graph_hooks + builtin + infra, key=lambda h: h.get("layer", 2)
    )


async def _run_hooks(
    event: str,
    context: Dict,
    hooks: List[Dict],
    project_path: str,
) -> Optional[Dict]:
    """Evaluate hooks for a graph event.

    Same evaluation logic as SafetyHarness.run_hooks():
    - Filter by event name
    - Evaluate condition via condition_evaluator.matches()
    - Interpolate action via interpolation.interpolate_action()
    - Dispatch via _dispatch_action()
    - Layer 1-2: first non-None result wins (control flow)
    - Layer 3: always runs (infra telemetry)
    """
    condition_evaluator = load_module(
        "loaders/condition_evaluator", anchor=_ANCHOR
    )
    interpolation = load_module("loaders/interpolation", anchor=_ANCHOR)

    control_result = None
    for hook in hooks:
        if hook.get("event") != event:
            continue
        if not condition_evaluator.matches(context, hook.get("condition", {})):
            continue

        action = hook.get("action", {})
        interpolated = interpolation.interpolate_action(action, context)
        result = await _dispatch_action(interpolated, project_path)

        if hook.get("layer") == 3:
            continue  # infra hooks don't affect control flow

        if result and control_result is None:
            unwrapped = _unwrap_result(result)
            if unwrapped is not None and unwrapped != {"success": True}:
                control_result = unwrapped

    return control_result


# ---------------------------------------------------------------------------
# Edge evaluation
# ---------------------------------------------------------------------------


def _evaluate_edges(
    next_spec: Any, state: Dict, result: Dict
) -> Optional[str]:
    """Evaluate edge conditions to determine the next node.

    next_spec can be:
    - str: unconditional edge
    - list: conditional edges, first match wins
    - None: terminal (graph ends)
    """
    condition_evaluator = load_module(
        "loaders/condition_evaluator", anchor=_ANCHOR
    )

    if next_spec is None:
        return None
    if isinstance(next_spec, str):
        return next_spec
    if isinstance(next_spec, list):
        doc = {"state": state, "result": result}
        for edge in next_spec:
            condition = edge.get("when")
            if condition is None:
                return edge.get("to")  # default edge
            if condition_evaluator.matches(doc, condition):
                return edge.get("to")
    return None


def _find_error_edge(node: Dict) -> Optional[str]:
    """Find the on_error target node for a node definition."""
    return node.get("on_error")


# ---------------------------------------------------------------------------
# State persistence (knowledge item)
# ---------------------------------------------------------------------------


async def _persist_state(
    project_path: str,
    graph_id: str,
    graph_run_id: str,
    state: Dict,
    current_node: Optional[str],
    status: str,
    step_count: int,
) -> None:
    """Write graph state as a signed knowledge item.

    Atomic write (temp → rename) to prevent corruption.
    """
    proj = Path(project_path)
    knowledge_dir = proj / AI_DIR / "knowledge" / "graphs" / graph_id
    knowledge_dir.mkdir(parents=True, exist_ok=True)
    knowledge_path = knowledge_dir / f"{graph_run_id}.md"

    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    parent_thread_id = os.environ.get("RYE_PARENT_THREAD_ID", "")

    frontmatter = (
        f"```yaml\n"
        f"id: graphs/{graph_id}/{graph_run_id}\n"
        f'title: "State: {graph_id} ({graph_run_id})"\n'
        f"entry_type: graph_state\n"
        f"category: graphs/{graph_id}\n"
        f'version: "1.0.0"\n'
        f"graph_id: {graph_id}\n"
        f"graph_run_id: {graph_run_id}\n"
        f"parent_thread_id: {parent_thread_id}\n"
        f"status: {status}\n"
        f"current_node: {current_node or ''}\n"
        f"step_count: {step_count}\n"
        f"updated_at: {now}\n"
        f"tags: [graph_state]\n"
        f"```\n\n"
    )

    body = json.dumps(state, indent=2, default=str)
    content = frontmatter + body

    signature = MetadataManager.create_signature(ItemType.KNOWLEDGE, content)
    signed_content = signature + content

    tmp_path = knowledge_path.with_suffix(".md.tmp")
    tmp_path.write_text(signed_content, encoding="utf-8")
    tmp_path.rename(knowledge_path)


# ---------------------------------------------------------------------------
# Graph validation
# ---------------------------------------------------------------------------


def _validate_graph(cfg: Dict, graph_config: Optional[Dict] = None) -> List[str]:
    """Validate graph definition before execution."""
    errors = []

    # Require permissions field at the graph top level
    if graph_config and not graph_config.get("permissions"):
        errors.append(
            "graph must declare 'permissions' — a list of capability tokens "
            "(e.g., ['rye.execute.tool.*']). Graphs without permissions cannot "
            "dispatch any actions."
        )

    nodes = cfg.get("nodes", {})
    start = cfg.get("start")

    if not start:
        errors.append("no 'start' node defined")
    elif start not in nodes:
        errors.append(f"start node '{start}' not found in nodes")

    has_return = False
    for name, node in nodes.items():
        if node.get("type") == "return":
            has_return = True
            continue

        # Check next references
        next_spec = node.get("next")
        if isinstance(next_spec, str):
            if next_spec not in nodes:
                errors.append(
                    f"node '{name}' references unknown node '{next_spec}'"
                )
        elif isinstance(next_spec, list):
            for edge in next_spec:
                to = edge.get("to")
                if to and to not in nodes:
                    errors.append(
                        f"node '{name}' edge references unknown node '{to}'"
                    )

        # Check on_error reference
        on_error = node.get("on_error")
        if on_error and on_error not in nodes:
            errors.append(
                f"node '{name}' on_error references unknown node '{on_error}'"
            )

    if not has_return and not errors:
        logger.warning("graph has no return node — will terminate on edge dead-end")

    return errors


# ---------------------------------------------------------------------------
# Input validation
# ---------------------------------------------------------------------------


def _validate_inputs(params: Dict, config_schema: Optional[Dict]) -> List[str]:
    """Validate input params against config_schema required fields."""
    if not config_schema:
        return []

    errors = []
    required = config_schema.get("required", [])
    for field in required:
        if field not in params:
            errors.append(f"missing required input: '{field}'")
    return errors


# ---------------------------------------------------------------------------
# Error context (same shape as runner.py)
# ---------------------------------------------------------------------------


def _error_to_context(result: Dict) -> Dict:
    """Convert an error result dict to context for error classification."""
    return {
        "error": {
            "type": "ToolExecutionError",
            "message": result.get("error", "unknown"),
            "code": result.get("code"),
        }
    }


# ---------------------------------------------------------------------------
# Continuation chain handling
# ---------------------------------------------------------------------------


def _follow_continuation_chain(
    continuation_id: str, project_path: str
) -> Dict:
    """Follow a continuation chain to the terminal thread's persisted result."""
    orchestrator = load_module("orchestrator", anchor=_ANCHOR)
    thread_registry = load_module(
        "persistence/thread_registry", anchor=_ANCHOR
    )

    terminal_id = orchestrator.resolve_thread_chain(
        continuation_id, Path(project_path)
    )
    registry = thread_registry.get_registry(Path(project_path))
    terminal_thread = registry.get_thread(terminal_id)

    if terminal_thread:
        persisted = terminal_thread.get("result", {})
        if isinstance(persisted, str):
            try:
                persisted = json.loads(persisted)
            except (json.JSONDecodeError, ValueError):
                persisted = {"result": persisted}
        return {
            **persisted,
            "status": terminal_thread.get("status", "completed"),
            "thread_id": terminal_id,
        }

    return {"status": "error", "error": f"Terminal thread not found: {terminal_id}"}


# ---------------------------------------------------------------------------
# Resume support
# ---------------------------------------------------------------------------


def _load_resume_state(
    project_path: str, graph_id: str, graph_run_id: str
) -> Optional[Dict]:
    """Load and verify a persisted graph state for resume.

    Returns dict with 'state', 'current_node', 'step_count' on success.
    Returns None if state file not found or signature invalid.
    """
    proj = Path(project_path)
    knowledge_path = (
        proj / AI_DIR / "knowledge" / "graphs" / graph_id / f"{graph_run_id}.md"
    )
    if not knowledge_path.exists():
        return None

    content = knowledge_path.read_text(encoding="utf-8")

    # Verify signature
    mm = MetadataManager()
    sig_result = mm.parse_and_verify(content)
    if sig_result and not sig_result.get("valid", True):
        logger.warning(
            "State signature invalid for %s/%s — cannot resume",
            graph_id, graph_run_id,
        )
        return None

    # Parse frontmatter for current_node and step_count
    current_node = None
    step_count = 0
    in_frontmatter = False
    body_lines = []
    frontmatter_done = False

    for line in content.split("\n"):
        stripped = line.strip()
        # Skip signature line
        if stripped.startswith("# rye:signed:") or stripped.startswith("<!-- rye:signed:"):
            continue
        if stripped == "```yaml" and not frontmatter_done:
            in_frontmatter = True
            continue
        if stripped == "```" and in_frontmatter:
            in_frontmatter = False
            frontmatter_done = True
            continue
        if in_frontmatter:
            if stripped.startswith("current_node:"):
                current_node = stripped.split(":", 1)[1].strip() or None
            elif stripped.startswith("step_count:"):
                try:
                    step_count = int(stripped.split(":", 1)[1].strip())
                except ValueError:
                    pass
        elif frontmatter_done:
            body_lines.append(line)

    # Parse body as JSON state
    body_text = "\n".join(body_lines).strip()
    if not body_text:
        return None

    try:
        state = json.loads(body_text)
    except (json.JSONDecodeError, ValueError):
        logger.warning("Cannot parse state JSON for %s/%s", graph_id, graph_run_id)
        return None

    return {
        "state": state,
        "current_node": current_node,
        "step_count": step_count,
    }


# ---------------------------------------------------------------------------
# Main execution
# ---------------------------------------------------------------------------


async def execute(
    graph_config: Dict, params: Dict, project_path: str,
    graph_run_id: Optional[str] = None,
    pre_registered: bool = False,
) -> Dict:
    """Walk a state graph, dispatching actions for each node."""
    interpolation = load_module("loaders/interpolation", anchor=_ANCHOR)
    error_loader = load_module("loaders/error_loader", anchor=_ANCHOR)
    thread_registry = load_module(
        "persistence/thread_registry", anchor=_ANCHOR
    )

    cfg = graph_config.get("config", {})
    nodes = cfg.get("nodes", {})
    max_steps = cfg.get("max_steps", 100)
    error_mode = cfg.get("on_error", "fail")

    # Derive IDs
    graph_id = graph_config.get("_item_id") or graph_config.get("category", "unknown")
    parent_thread_id = os.environ.get("RYE_PARENT_THREAD_ID")
    is_resume = params.pop("resume", False)
    resume_run_id = params.pop("graph_run_id", None)

    # Resolve execution context
    exec_ctx = _resolve_execution_context(params, project_path, graph_config)

    # Merge hooks
    hooks = _merge_graph_hooks(cfg.get("hooks", []), project_path)

    # Validate graph
    validation_errors = _validate_graph(cfg, graph_config)
    if validation_errors:
        return {
            "success": False,
            "error": f"Graph validation failed: {validation_errors}",
        }

    # Resume: reload state from signed knowledge item
    if is_resume and resume_run_id:
        graph_run_id = resume_run_id
        resumed = _load_resume_state(project_path, graph_id, graph_run_id)
        if not resumed:
            return {
                "success": False,
                "error": f"Cannot resume: state not found or signature invalid for {graph_id}/{graph_run_id}",
            }
        state = resumed["state"]
        current = resumed["current_node"]
        step_count = resumed["step_count"]

        if not current:
            return {
                "success": False,
                "error": f"Cannot resume: no current_node in state for {graph_run_id}",
            }

        registry = thread_registry.get_registry(Path(project_path))
        registry.update_status(graph_run_id, "running")
        await _persist_state(
            project_path, graph_id, graph_run_id,
            state, current, "running", step_count,
        )
    else:
        # Fresh run
        if not graph_run_id:
            graph_run_id = f"{graph_id.replace('/', '-')}-{int(time.time())}"
        state: Dict[str, Any] = {"inputs": params}
        current = cfg.get("start")
        step_count = 0

        # Validate inputs
        config_schema = graph_config.get("config_schema")
        input_errors = _validate_inputs(params, config_schema)
        if input_errors:
            return {
                "success": False,
                "error": f"Input validation failed: {input_errors}",
            }

        # Register + create initial state
        # (skip register if graph_run_id was pre-provided — already registered
        # by run_sync() for async)
        registry = thread_registry.get_registry(Path(project_path))
        if not pre_registered:
            registry.register(graph_run_id, graph_id, parent_thread_id)
            registry.update_status(graph_run_id, "running")
        await _persist_state(
            project_path, graph_id, graph_run_id,
            state, current, "running", step_count,
        )

        # Fire graph_started hooks (only on fresh runs)
        await _run_hooks(
            "graph_started",
            {"graph_id": graph_id, "state": state},
            hooks,
            project_path,
        )

    while current and step_count < max_steps:
        node = nodes.get(current)
        if node is None:
            registry.update_status(graph_run_id, "error")
            return {
                "success": False,
                "error": f"Node '{current}' not found in graph",
                "state": state,
            }

        step_count += 1
        executed_node = current

        # Return node — terminate
        if node.get("type") == "return":
            await _persist_state(
                project_path, graph_id, graph_run_id,
                state, current, "completed", step_count,
            )
            registry.update_status(graph_run_id, "completed")
            await _run_hooks(
                "graph_completed",
                {"graph_id": graph_id, "state": state, "steps": step_count},
                hooks,
                project_path,
            )
            return {"success": True, "state": state, "steps": step_count}

        # Foreach node — iterate
        if node.get("type") == "foreach":
            current, state = await _handle_foreach(
                node, state, params, exec_ctx, project_path
            )
            await _persist_state(
                project_path, graph_id, graph_run_id,
                state, current, "running", step_count,
            )
            continue

        # Build interpolation context
        interp_ctx: Dict[str, Any] = {"state": state, "inputs": params}

        # Interpolate action params from state
        action = interpolation.interpolate_action(node["action"], interp_ctx)

        # Inject parent context for thread_directive calls
        if action.get("item_id") == "rye/agent/threads/thread_directive":
            action["params"] = _inject_parent_context(
                action.get("params", {}), exec_ctx
            )

        # Check capabilities before dispatch
        denied = _check_permission(
            exec_ctx,
            action.get("primary", "execute"),
            action.get("item_type", "tool"),
            action.get("item_id", ""),
        )
        if denied:
            result = denied
        else:
            raw_result = await _dispatch_action(action, project_path)
            result = _unwrap_result(raw_result)

        # Handle continuation chains for LLM nodes
        if (
            action.get("item_id") == "rye/agent/threads/thread_directive"
            and result.get("status") == "continued"
            and result.get("continuation_thread_id")
        ):
            result = _follow_continuation_chain(
                result["continuation_thread_id"], project_path
            )

        # Check for errors — hooks get first chance
        if result.get("status") == "error":
            classification = error_loader.classify(
                Path(project_path), _error_to_context(result)
            )
            error_ctx = {
                "error": result,
                "classification": classification,
                "node": executed_node,
                "state": state,
                "step_count": step_count,
            }
            hook_action = await _run_hooks(
                "error", error_ctx, hooks, project_path
            )
            if hook_action and hook_action.get("action") == "retry":
                max_retries = hook_action.get("max_retries", 3)
                retries = state.get("_retries", {}).get(executed_node, 0)
                if retries < max_retries:
                    state.setdefault("_retries", {})[executed_node] = retries + 1
                    step_count -= 1
                    continue

            state["_last_error"] = {
                "node": executed_node,
                "error": result.get("error", "unknown"),
            }
            error_edge = _find_error_edge(node)
            if error_edge:
                current = error_edge
                await _persist_state(
                    project_path, graph_id, graph_run_id,
                    state, current, "running", step_count,
                )
                continue
            if error_mode == "fail":
                await _persist_state(
                    project_path, graph_id, graph_run_id,
                    state, current, "error", step_count,
                )
                registry.update_status(graph_run_id, "error")
                return {
                    "success": False,
                    "error": result.get("error"),
                    "node": executed_node,
                    "state": state,
                }
            # error_mode == "continue" — skip assign, proceed to edges

        # Assign result values to state (skipped on error in "continue" mode)
        if result.get("status") != "error":
            interp_ctx["result"] = result
            if "assign" in node:
                for key, expr in node["assign"].items():
                    resolved = interpolation.interpolate(expr, interp_ctx)
                    if resolved is None and expr:
                        logger.warning(
                            "assign '%s' resolved to None for expr '%s'",
                            key, expr,
                        )
                    state[key] = resolved

        # Evaluate edges
        next_spec = node.get("next")
        current = _evaluate_edges(next_spec, state, result)

        # Persist + sign state after each step
        await _persist_state(
            project_path, graph_id, graph_run_id,
            state, current, "running", step_count,
        )

        # Fire after_step hooks
        await _run_hooks(
            "after_step",
            {
                "node": executed_node,
                "next_node": current,
                "state": state,
                "step_count": step_count,
                "result": result,
            },
            hooks,
            project_path,
        )

        # Cancellation check
        cancel_path = (
            Path(project_path) / AI_DIR / "threads" / graph_run_id / "cancel"
        )
        if cancel_path.exists():
            await _persist_state(
                project_path, graph_id, graph_run_id,
                state, current, "cancelled", step_count,
            )
            registry.update_status(graph_run_id, "cancelled")
            return {
                "success": False,
                "status": "cancelled",
                "state": state,
                "steps": step_count,
            }

    # Max steps exceeded
    limit_ctx = {
        "limit_code": "max_steps_exceeded",
        "current_value": step_count,
        "current_max": max_steps,
        "state": state,
    }
    await _run_hooks("limit", limit_ctx, hooks, project_path)

    await _persist_state(
        project_path, graph_id, graph_run_id,
        state, current, "error", step_count,
    )
    registry.update_status(graph_run_id, "error")
    await _run_hooks(
        "graph_completed",
        {
            "graph_id": graph_id,
            "state": state,
            "steps": step_count,
            "error": "max_steps_exceeded",
        },
        hooks,
        project_path,
    )
    return {
        "success": False,
        "error": f"Max steps exceeded ({max_steps})",
        "state": state,
    }


# ---------------------------------------------------------------------------
# Foreach support
# ---------------------------------------------------------------------------


async def _handle_foreach(
    node: Dict,
    state: Dict,
    inputs: Dict,
    exec_ctx: Dict,
    project_path: str,
) -> tuple:
    """Handle a foreach node — iterate over a list, execute action per item.

    Parallel mode: when the inner action contains async: true (e.g. a
    thread_directive call), all iterations are dispatched concurrently via
    asyncio.gather.  Sequential mode (default): each iteration completes
    before the next starts.

    Returns (next_node, updated_state).
    """
    interpolation = load_module("loaders/interpolation", anchor=_ANCHOR)

    interp_ctx: Dict[str, Any] = {"state": state, "inputs": inputs}
    over_expr = node.get("over", "")
    items = interpolation.interpolate(over_expr, interp_ctx)
    if not isinstance(items, list):
        items = []

    as_var = node.get("as", "item")
    collect_var = node.get("collect")

    # Detect parallel mode: check if the raw action template has async
    raw_params = node.get("action", {}).get("params", {})
    is_parallel = raw_params.get("async") is True

    if is_parallel:
        collected = await _foreach_parallel(
            node, items, as_var, inputs, exec_ctx, project_path
        )
    else:
        collected = await _foreach_sequential(
            node, items, as_var, state, inputs, exec_ctx, project_path
        )

    if collect_var:
        state[collect_var] = collected

    # Clean up iteration variable
    state.pop(as_var, None)

    next_node = _evaluate_edges(node.get("next"), state, {})
    return next_node, state


async def _foreach_sequential(
    node: Dict,
    items: List,
    as_var: str,
    state: Dict,
    inputs: Dict,
    exec_ctx: Dict,
    project_path: str,
) -> List:
    """Execute foreach items one at a time."""
    interpolation = load_module("loaders/interpolation", anchor=_ANCHOR)
    collected: List[Any] = []

    for item in items:
        state[as_var] = item
        interp_ctx: Dict[str, Any] = {
            "state": state, "inputs": inputs, as_var: item,
        }

        action = interpolation.interpolate_action(node["action"], interp_ctx)

        if action.get("item_id") == "rye/agent/threads/thread_directive":
            action["params"] = _inject_parent_context(
                action.get("params", {}), exec_ctx
            )

        denied = _check_permission(
            exec_ctx,
            action.get("primary", "execute"),
            action.get("item_type", "tool"),
            action.get("item_id", ""),
        )
        if denied:
            result = denied
        else:
            raw_result = await _dispatch_action(action, project_path)
            result = _unwrap_result(raw_result)

        collected.append(result.get("thread_id", result))

    return collected


async def _foreach_parallel(
    node: Dict,
    items: List,
    as_var: str,
    inputs: Dict,
    exec_ctx: Dict,
    project_path: str,
) -> List:
    """Dispatch all foreach items concurrently via asyncio.gather."""
    interpolation = load_module("loaders/interpolation", anchor=_ANCHOR)

    async def _run_one(item: Any) -> Any:
        interp_ctx: Dict[str, Any] = {
            "state": {"inputs": inputs, as_var: item},
            "inputs": inputs,
            as_var: item,
        }
        action = interpolation.interpolate_action(node["action"], interp_ctx)

        if action.get("item_id") == "rye/agent/threads/thread_directive":
            action["params"] = _inject_parent_context(
                action.get("params", {}), exec_ctx
            )

        denied = _check_permission(
            exec_ctx,
            action.get("primary", "execute"),
            action.get("item_type", "tool"),
            action.get("item_id", ""),
        )
        if denied:
            return denied
        raw_result = await _dispatch_action(action, project_path)
        result = _unwrap_result(raw_result)
        return result.get("thread_id", result)

    return list(await asyncio.gather(*[_run_one(item) for item in items]))


# ---------------------------------------------------------------------------
# Sync entry point with async support
# ---------------------------------------------------------------------------


def run_sync(
    graph_config: Dict, params: Dict, project_path: str
) -> Dict:
    """Synchronous entry point for graph execution.

    Supports ``async`` parameter: when True, forks a child process
    that runs the graph in the background.  The parent returns immediately
    with ``{success, graph_run_id, status: "running"}``.

    Same pattern as thread_directive.py async.
    """
    thread_registry = load_module(
        "persistence/thread_registry", anchor=_ANCHOR
    )

    is_async = params.pop("async", False)

    if is_async:
        # Pre-generate graph_run_id so parent can return it
        cfg = graph_config.get("config", {})
        graph_id = graph_config.get("_item_id") or graph_config.get("category", "unknown")
        graph_run_id = f"{graph_id.replace('/', '-')}-{int(time.time())}"

        # Register before fork so both parent and child see it
        parent_thread_id = os.environ.get("RYE_PARENT_THREAD_ID")
        registry = thread_registry.get_registry(Path(project_path))
        registry.register(graph_run_id, graph_id, parent_thread_id)
        registry.update_status(graph_run_id, "running")

        child_pid = os.fork()
        if child_pid == 0:
            # Child process — run the graph to completion
            try:
                os.setsid()

                # Redirect stdio to log file for debugging, devnull as fallback
                log_dir = Path(project_path) / AI_DIR / "threads" / graph_run_id
                log_dir.mkdir(parents=True, exist_ok=True)
                log_fd = os.open(
                    str(log_dir / "async.log"),
                    os.O_WRONLY | os.O_CREAT | os.O_TRUNC,
                    0o644,
                )
                devnull = os.open(os.devnull, os.O_RDWR)
                os.dup2(devnull, 0)
                os.dup2(log_fd, 2)  # stderr → log
                os.dup2(devnull, 1)  # stdout → devnull (prevent corrupting parent)
                os.close(devnull)
                os.close(log_fd)

                asyncio.run(execute(
                    graph_config, params, project_path,
                    graph_run_id=graph_run_id,
                    pre_registered=True,
                ))
            except Exception:
                import traceback
                traceback.print_exc()  # goes to async.log via stderr
                try:
                    registry.update_status(graph_run_id, "error")
                except Exception:
                    pass
            finally:
                os._exit(0)
        else:
            # Parent — return immediately
            return {
                "success": True,
                "graph_run_id": graph_run_id,
                "graph_id": graph_id,
                "status": "running",
                "pid": child_pid,
            }

    # Synchronous execution
    return asyncio.run(execute(graph_config, params, project_path))


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--graph-path", required=True)
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()

    if os.environ.get("RYE_DEBUG"):
        logging.basicConfig(
            level=logging.DEBUG,
            format="[%(name)s] %(levelname)s: %(message)s",
            stream=__import__("sys").stderr,
        )

    graph_config = _load_graph_yaml(args.graph_path)
    result = run_sync(graph_config, json.loads(args.params), args.project_path)
    print(json.dumps(result, default=str))
