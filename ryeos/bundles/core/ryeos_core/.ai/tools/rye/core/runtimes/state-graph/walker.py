# rye:signed:2026-02-28T00:25:41Z:586ae693405541c468a0cf9799d465104ff054480090549baac02af489dce10f:0cGbug8EW22YOj1PGbwIemtWGFP5Zqt2l87BQIc8Z-JkFffXWXa2l0uNG8vGWczgeWpleo9Ik6HPBV2gpLGmBg==:4b987fd4e40303ac
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
import platform
import re
import subprocess
import sys
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
import condition_evaluator
import interpolation

logger = logging.getLogger(__name__)

def _find_tools_root() -> Path:
    """Walk up from __file__ to find the .ai/tools boundary for this bundle."""
    current = Path(__file__).resolve().parent
    while current != current.parent:
        if current.name == "tools" and current.parent.name == ".ai":
            return current
        current = current.parent
    raise RuntimeError(
        f"Cannot find .ai/tools root from {__file__} — "
        "walker.py must live under a .ai/tools/ directory"
    )


def _find_agent_threads_anchor() -> Optional[Path]:
    """Resolve rye/agent/threads across system bundles.

    The walker lives in core but rye/agent/threads is in the standard bundle.
    Returns None when the standard bundle is not installed (serverless/core-only).
    """
    # Check own bundle first
    own = _find_tools_root() / "rye" / "agent" / "threads"
    if own.is_dir():
        return own
    # Search across installed system bundles
    try:
        from rye.utils.path_utils import get_system_spaces
        for bundle in get_system_spaces():
            candidate = bundle.root_path / AI_DIR / "tools" / "rye" / "agent" / "threads"
            if candidate.is_dir():
                return candidate
    except Exception:
        pass
    return None


def _try_load_module(relative_path: str) -> Optional[Any]:
    """Load a module from _ANCHOR, returning None if agent bundle unavailable."""
    if _ANCHOR is None:
        return None
    try:
        return load_module(relative_path, anchor=_ANCHOR)
    except FileNotFoundError:
        return None


_TOOLS_ROOT = _find_tools_root()
_ANCHOR = _find_agent_threads_anchor()


# ---------------------------------------------------------------------------
# Graph transcript — JSONL event log + signed knowledge markdown
# ---------------------------------------------------------------------------

class GraphTranscript:
    """JSONL event log + signed knowledge markdown for graph execution.

    Two outputs, same pattern as thread Transcript:

    1. transcript.jsonl — append-only events, ``tail -f`` friendly
       Path: {project}/.ai/agent/graphs/{graph_run_id}/transcript.jsonl

    2. knowledge markdown — visual node status table + event history,
       re-rendered from JSONL at step boundaries, signed
       Path: {project}/.ai/knowledge/agent/graphs/{graph_id}/{graph_run_id}.md

    No SSE streaming — graphs don't produce tokens.
    """

    def __init__(
        self, project_path: str, graph_id: str, graph_run_id: str,
        nodes_config: Dict,
    ):
        self._project_path = Path(project_path)
        self._graph_id = graph_id
        self._graph_run_id = graph_run_id
        self._nodes_config = nodes_config

        # JSONL directory (graphs live under agent/graphs/, not agent/threads/)
        self._thread_dir = (
            self._project_path / AI_DIR / "agent" / "graphs" / graph_run_id
        )
        self._thread_dir.mkdir(parents=True, exist_ok=True)
        self._jsonl_path = self._thread_dir / "transcript.jsonl"

    # -- Event log (append-only JSONL) --

    def write_event(self, event_type: str, payload: Dict) -> None:
        """Append event to JSONL file, flushed immediately."""
        entry = {
            "timestamp": time.time(),
            "thread_id": self._graph_run_id,
            "event_type": event_type,
            "payload": payload,
        }
        with open(self._jsonl_path, "a") as f:
            f.write(json.dumps(entry, default=str) + "\n")
            f.flush()

    def checkpoint(
        self, step: int, *, state: Optional[Dict] = None,
        current_node: Optional[str] = None,
    ) -> None:
        """Sign transcript JSONL at step boundary via TranscriptSigner.

        If state is provided, emits a state_snapshot event before the
        checkpoint so resume can reconstruct entirely from the jsonl.
        """
        if state is not None:
            self.write_event("state_snapshot", {
                "step": step,
                "current_node": current_node,
                "state": state,
            })
        transcript_signer = _try_load_module("persistence/transcript_signer")
        if transcript_signer is None:
            return
        signer = transcript_signer.TranscriptSigner(
            self._graph_run_id, self._thread_dir
        )
        signer.checkpoint(step)

    # -- Knowledge markdown (visual state + event history) --

    def render_knowledge(
        self, status: str = "running", step_count: int = 0,
        total_elapsed_s: float = 0,
    ) -> Optional[Path]:
        """Render signed knowledge markdown from JSONL events.

        Produces a markdown file with:
        1. YAML frontmatter (graph metadata)
        2. Visual node status table (✅/🔄/⏳/❌)
        3. Step-by-step event history
        4. Footer with status summary

        Overwritten from the JSONL source of truth at each step.
        """
        events = self._read_events()
        if not events:
            return None

        # Derive per-node state from events
        node_results: Dict[str, Dict] = {}
        current_running: Optional[str] = None
        for event in events:
            et = event.get("event_type", "")
            p = event.get("payload", {})
            if et == "step_started":
                current_running = p.get("node")
            elif et == "step_completed":
                node = p.get("node")
                node_results[node] = {
                    "status": "error" if p.get("status") == "error" else "completed",
                    "elapsed_s": p.get("elapsed_s", 0),
                    "action_id": p.get("action_id", ""),
                    "thread_id": p.get("thread_id", ""),
                    "step": p.get("step", 0),
                }
                if current_running == node:
                    current_running = None
            elif et == "foreach_completed":
                node = p.get("node")
                node_results[node] = {
                    "status": "completed",
                    "elapsed_s": 0,
                    "action_id": "",
                    "thread_id": "",
                    "step": p.get("step", 0),
                }
                if current_running == node:
                    current_running = None

        created_at = ""
        for e in events:
            if e.get("timestamp"):
                created_at = datetime.fromtimestamp(
                    e["timestamp"], tz=timezone.utc
                ).strftime("%Y-%m-%dT%H:%M:%SZ")
                break

        if total_elapsed_s >= 60:
            duration_str = f"{total_elapsed_s / 60:.1f}m"
        else:
            duration_str = f"{total_elapsed_s:.1f}s"

        category = f"agent/graphs/{self._graph_id}"
        parts: List[str] = []

        # Frontmatter
        parts.append(
            f"```yaml\n"
            f"id: {self._graph_run_id}\n"
            f'title: "Graph: {self._graph_id}"\n'
            f"entry_type: graph_transcript\n"
            f"category: {category}\n"
            f'version: "1.0.0"\n'
            f"author: rye\n"
            f"created_at: {created_at}\n"
            f"graph_id: {self._graph_id}\n"
            f"graph_run_id: {self._graph_run_id}\n"
            f"status: {status}\n"
            f"step_count: {step_count}\n"
            f"duration: {duration_str}\n"
            f"tags: [graph, {status}]\n"
            f"```\n\n"
        )

        # Title + summary line
        parts.append(f"# {self._graph_id}\n\n")
        parts.append(
            f"**Status:** {status} | **Step:** {step_count}"
            f" | **Elapsed:** {duration_str}\n\n"
        )

        # Visual node status table
        parts.append("| # | Node | Status | Duration | Action | Details |\n")
        parts.append("|---|------|--------|----------|--------|---------|\n")
        for node_name in self._nodes_config:
            if node_name in node_results:
                nr = node_results[node_name]
                icon = "✅" if nr["status"] == "completed" else "❌"
                dur = f'{nr["elapsed_s"]:.1f}s'
                action = nr["action_id"]
                details = (
                    f'thread: `{nr["thread_id"]}`' if nr["thread_id"] else ""
                )
                parts.append(
                    f'| {nr["step"]} | {node_name} | {icon}'
                    f" | {dur} | {action} | {details} |\n"
                )
            elif node_name == current_running:
                parts.append(f"| — | {node_name} | 🔄 | — | | |\n")
            else:
                parts.append(f"| — | {node_name} | ⏳ | — | | |\n")
        parts.append("\n---\n\n")

        # Event history
        for event in events:
            chunk = self._render_event(event)
            if chunk:
                parts.append(chunk)

        # Footer
        labels = {
            "completed": "✅ Completed", "error": "❌ Error",
            "cancelled": "⏹ Cancelled", "running": "🔄 Running",
        }
        label = labels.get(status, status.title())
        now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        parts.append(
            f"---\n\n**{label}** — {step_count} steps,"
            f" {duration_str}, {now}\n"
        )

        content = "".join(parts)

        # Sign and write
        from rye.constants import ItemType
        knowledge_dir = (
            self._project_path / AI_DIR / "knowledge"
            / "agent" / "graphs" / self._graph_id
        )
        knowledge_dir.mkdir(parents=True, exist_ok=True)
        knowledge_path = knowledge_dir / f"{self._graph_run_id}-transcript.md"

        signature = MetadataManager.create_signature(ItemType.KNOWLEDGE, content)
        signed_content = signature + content
        knowledge_path.write_text(signed_content, encoding="utf-8")
        return knowledge_path

    def _read_events(self) -> List[Dict]:
        """Read all non-checkpoint events from JSONL."""
        if not self._jsonl_path.exists():
            return []
        events: List[Dict] = []
        with open(self._jsonl_path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                    if event.get("event_type") != "checkpoint":
                        events.append(event)
                except json.JSONDecodeError:
                    continue
        return events

    @staticmethod
    def _render_event(event: Dict) -> str:
        """Render a single graph event as markdown."""
        et = event.get("event_type", "")
        p = event.get("payload", {})

        if et == "graph_started":
            ts = datetime.fromtimestamp(
                event["timestamp"], tz=timezone.utc
            ).strftime("%Y-%m-%dT%H:%M:%SZ")
            return (
                f"**Started** {ts} — entry:"
                f" `{p.get('start_node', '')}`\n\n"
            )

        if et == "step_started":
            step = p.get("step", 0)
            node = p.get("node", "")
            node_type = p.get("node_type", "")
            action_id = p.get("action_id", "")
            if node_type == "return":
                return f"### Step {step} — `{node}` ⏹ return\n\n"
            if node_type == "foreach":
                return f"### Step {step} — `{node}` 🔁 foreach\n\n"
            if action_id:
                return f"### Step {step} — `{node}` → {action_id}\n\n"
            return f"### Step {step} — `{node}`\n\n"

        if et == "step_completed":
            elapsed_s = p.get("elapsed_s", 0)
            status = p.get("status", "ok")
            thread_id = p.get("thread_id", "")
            next_node = p.get("next_node")
            error = p.get("error", "")

            if status != "error":
                line = f"✅ completed ({elapsed_s:.1f}s)"
            else:
                line = f"❌ error ({elapsed_s:.1f}s): {error}"
            if thread_id:
                line += f" — thread: `{thread_id}`"
            if next_node:
                line += f" → `{next_node}`"
            return line + "\n\n"

        if et == "foreach_completed":
            next_node = p.get("next_node")
            if next_node:
                return f"🔁 iteration complete → `{next_node}`\n\n"
            return "🔁 iteration complete\n\n"

        return ""


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
    meta_path = Path(project_path) / AI_DIR / "agent" / "threads" / thread_id / "thread.json"
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
            transcript_signer = _try_load_module("persistence/transcript_signer")
            if transcript_signer is None:
                return {
                    "parent_thread_id": None,
                    "capabilities": [],
                    "limits": {},
                }
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
    hooks_loader = _try_load_module("loaders/hooks_loader")
    if hooks_loader is None:
        return []
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
    knowledge_dir = proj / AI_DIR / "knowledge" / "agent" / "graphs" / graph_id
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

    _KNOWN_NODE_KEYS = frozenset({
        "type", "action", "next", "on_error", "assign",
        "over", "as", "collect", "parallel", "env_requires",
    })

    has_return = False
    for name, node in nodes.items():
        if node.get("type") == "return":
            has_return = True
            continue

        # Warn on unknown node-level keys
        unknown = set(node.keys()) - _KNOWN_NODE_KEYS
        if unknown:
            logger.warning(
                "node '%s' has unknown keys: %s", name, ", ".join(sorted(unknown)),
            )

        # Warn on deprecated async placement in foreach nodes
        if node.get("type") == "foreach":
            if node.get("action", {}).get("async") is True:
                errors.append(
                    f"node '{name}': 'action.async' is not supported — "
                    f"use 'parallel: true' at node level"
                )
            if node.get("action", {}).get("params", {}).get("async") is True:
                errors.append(
                    f"node '{name}': 'action.params.async' is not supported — "
                    f"use 'parallel: true' at node level"
                )

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


_STATE_REF_RE = re.compile(r"\$\{state\.(\w+)")


def _analyze_graph(
    cfg: Dict, graph_config: Optional[Dict] = None
) -> tuple:
    """Static analysis of graph structure. Returns (errors, warnings).

    Extends _validate_graph with reachability analysis and state flow checks.
    """
    errors = _validate_graph(cfg, graph_config)
    warnings: List[str] = []

    nodes = cfg.get("nodes", {})
    start = cfg.get("start")
    if not start or start not in nodes:
        return errors, warnings

    # BFS reachability from start
    reachable: set = set()
    queue = [start]
    while queue:
        n = queue.pop(0)
        if n in reachable or n not in nodes:
            continue
        reachable.add(n)
        node = nodes[n]
        next_spec = node.get("next")
        if isinstance(next_spec, str):
            queue.append(next_spec)
        elif isinstance(next_spec, list):
            for edge in next_spec:
                if edge.get("to"):
                    queue.append(edge["to"])
        on_error = node.get("on_error")
        if on_error:
            queue.append(on_error)

    unreachable = set(nodes.keys()) - reachable
    if unreachable:
        warnings.append(f"unreachable nodes: {', '.join(sorted(unreachable))}")

    # State flow analysis (best-effort)
    assigned: set = set()
    referenced: set = set()

    for name, node in nodes.items():
        # Assigned: from assign blocks and collect vars
        for key in node.get("assign", {}).keys():
            assigned.add(key)
        collect = node.get("collect")
        if collect:
            assigned.add(collect)

        # Referenced: scan all string values for ${state.X}
        node_json = json.dumps(node, default=str)
        for match in _STATE_REF_RE.findall(node_json):
            referenced.add(match)

    # Initial state keys count as assigned
    initial_state = cfg.get("state", {})
    for key in initial_state:
        assigned.add(key)

    # "inputs" is always available
    assigned.add("inputs")
    assigned.add("_last_error")
    assigned.add("_retries")

    ref_not_assigned = referenced - assigned
    if ref_not_assigned:
        warnings.append(
            f"state keys referenced but never assigned: {', '.join(sorted(ref_not_assigned))}"
        )

    assigned_not_ref = assigned - referenced - {"inputs", "_last_error", "_retries"}
    if assigned_not_ref:
        warnings.append(
            f"state keys assigned but never referenced: {', '.join(sorted(assigned_not_ref))}"
        )

    # Foreach structural checks
    for name, node in nodes.items():
        if node.get("type") == "foreach":
            if not node.get("over"):
                errors.append(f"foreach node '{name}' missing 'over' expression")
            if "action" not in node:
                errors.append(f"foreach node '{name}' missing 'action'")

    return errors, warnings


# ---------------------------------------------------------------------------
# Environment pre-validation
# ---------------------------------------------------------------------------


def _preflight_env_check(cfg: Dict, graph_config: Optional[Dict] = None) -> List[str]:
    """Check that required env vars for all graph tools are present.

    Sources of env requirements:
    1. Node-level ``env_requires`` lists (declared in graph YAML)
    2. Graph-level ``env_requires`` (applies to all nodes)

    Returns list of missing env var descriptions.
    """
    missing: List[str] = []
    seen_vars: set = set()

    # Graph-level env_requires
    graph_env = graph_config.get("env_requires", []) if graph_config else []
    for var in graph_env:
        if var not in os.environ and var not in seen_vars:
            missing.append(f"graph requires '{var}'")
            seen_vars.add(var)

    # Node-level env_requires
    nodes = cfg.get("nodes", {})
    for name, node in nodes.items():
        node_env = node.get("env_requires", [])
        if isinstance(node_env, str):
            node_env = [node_env]
        for var in node_env:
            if var not in os.environ and var not in seen_vars:
                tool_id = node.get("action", {}).get("item_id", "")
                missing.append(f"node '{name}' ({tool_id}) requires '{var}'")
                seen_vars.add(var)

    return missing


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
    orchestrator = _try_load_module("orchestrator")
    thread_registry = _try_load_module("persistence/thread_registry")
    if orchestrator is None or thread_registry is None:
        return {"success": False, "error": "Agent bundle required for continuation chain resolution"}

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

    Source of truth is the transcript.jsonl in the graph run directory.
    Integrity is verified via the last checkpoint (same TranscriptSigner
    used by thread transcripts). State is reconstructed from the last
    state_snapshot event in the jsonl.

    Returns dict with 'state', 'current_node', 'step_count' on success.
    Returns None if transcript not found or integrity check fails.
    """
    proj = Path(project_path)
    jsonl_path = proj / AI_DIR / "agent" / "graphs" / graph_run_id / "transcript.jsonl"
    if not jsonl_path.exists():
        logger.warning("No transcript.jsonl for %s — cannot resume", graph_run_id)
        return None

    # Verify transcript integrity via checkpoint signatures
    transcript_signer = _try_load_module("persistence/transcript_signer")
    if transcript_signer is None:
        return None
    signer = transcript_signer.TranscriptSigner(graph_run_id, jsonl_path.parent)
    verify_result = signer.verify(allow_unsigned_trailing=True)
    if not verify_result.get("valid", False):
        logger.warning(
            "Transcript integrity failed for %s: %s",
            graph_run_id, verify_result.get("error", "unknown"),
        )
        return None

    # Find the last state_snapshot event
    last_snapshot = None
    with open(jsonl_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("event_type") == "state_snapshot":
                last_snapshot = event.get("payload", {})

    if not last_snapshot or "state" not in last_snapshot:
        logger.warning("No state_snapshot in transcript for %s", graph_run_id)
        return None

    return {
        "state": last_snapshot["state"],
        "current_node": last_snapshot.get("current_node"),
        "step_count": last_snapshot.get("step", 0),
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
    error_loader = _try_load_module("loaders/error_loader")
    thread_registry = _try_load_module("persistence/thread_registry")

    cfg = graph_config.get("config", {})
    nodes = cfg.get("nodes", {})
    max_steps = cfg.get("max_steps", 100)
    error_mode = cfg.get("on_error", "fail")

    # Derive IDs — resolve _item_id from _file_path if not set
    if not graph_config.get("_item_id") and graph_config.get("_file_path"):
        fp = Path(graph_config["_file_path"]).resolve()
        # Walk up to find .ai/tools boundary and derive item_id
        for parent in fp.parents:
            if parent.name == "tools" and parent.parent.name == ".ai":
                try:
                    graph_config["_item_id"] = str(fp.relative_to(parent).with_suffix(""))
                except ValueError:
                    pass
                break
    graph_id = graph_config.get("_item_id") or graph_config.get("category", "unknown")
    parent_thread_id = os.environ.get("RYE_PARENT_THREAD_ID")
    is_resume = params.pop("resume", False)
    resume_run_id = params.pop("graph_run_id", None)

    # Single-step and validate mode params
    target_node = params.pop("node", None)
    inject_state = params.pop("inject_state", None)
    validate_only = params.pop("validate", False)

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

    # Validate-only mode — static analysis, no execution
    if validate_only:
        errors, warnings = _analyze_graph(cfg, graph_config)
        return {
            "success": len(errors) == 0,
            "errors": errors,
            "warnings": warnings,
            "node_count": len(nodes),
        }

    # Environment pre-validation
    missing_env = _preflight_env_check(cfg, graph_config)
    if missing_env:
        return {
            "success": False,
            "error": f"Missing environment variables: {missing_env}",
        }

    # Validate target_node exists
    if target_node and target_node not in nodes:
        return {
            "success": False,
            "error": f"Target node '{target_node}' not found in graph",
        }

    registry = None

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

        if thread_registry is not None:
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
        # Merge initial state from config.state, then overlay inputs
        initial_state = cfg.get("state", {})
        state: Dict[str, Any] = {**initial_state, "inputs": params}
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
        if thread_registry is not None:
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

    # Single-step mode: overlay injected state and jump to target node
    if inject_state:
        state.update(inject_state)
    if target_node:
        current = target_node
        if not graph_run_id or not graph_run_id.endswith("-step"):
            graph_run_id = f"{graph_id.replace('/', '-')}-{int(time.time())}-step"

    # Create graph transcript (JSONL event log + signed knowledge markdown)
    graph_transcript = GraphTranscript(project_path, graph_id, graph_run_id, nodes)
    graph_start_time = time.monotonic()
    if not is_resume:
        graph_transcript.write_event("graph_started", {
            "graph_id": graph_id, "graph_run_id": graph_run_id,
            "start_node": current or "",
        })
        graph_transcript.render_knowledge("running", step_count, 0)

    while current and step_count < max_steps:
        node = nodes.get(current)
        if node is None:
            elapsed = time.monotonic() - graph_start_time
            graph_transcript.write_event("graph_error", {
                "error": f"Node '{current}' not found", "steps": step_count,
                "elapsed_s": elapsed,
            })
            graph_transcript.checkpoint(step_count, state=state, current_node=current)
            graph_transcript.render_knowledge("error", step_count, elapsed)
            if registry is not None:
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
            elapsed = time.monotonic() - graph_start_time
            graph_transcript.write_event("step_started", {
                "step": step_count, "node": executed_node, "node_type": "return",
            })
            graph_transcript.write_event("step_completed", {
                "step": step_count, "node": executed_node, "status": "completed",
                "elapsed_s": 0, "action_id": "", "thread_id": "",
            })
            _log_progress(graph_id, step_count, len(nodes), executed_node, elapsed_s=elapsed, status="return")
            graph_transcript.write_event("graph_completed", {
                "status": "completed", "steps": step_count, "elapsed_s": elapsed,
            })
            graph_transcript.checkpoint(step_count, state=state, current_node=current)
            graph_transcript.render_knowledge("completed", step_count, elapsed)
            await _persist_state(
                project_path, graph_id, graph_run_id,
                state, current, "completed", step_count,
            )
            if registry is not None:
                registry.update_status(graph_run_id, "completed")
            await _run_hooks(
                "graph_completed",
                {"graph_id": graph_id, "state": state, "steps": step_count},
                hooks,
                project_path,
            )
            _log_progress(graph_id, step_count, len(nodes), "done", elapsed_s=elapsed, status="ok", detail=f"{step_count} steps")
            # Return interpolated output from the return node (slim),
            # full state is already persisted as a knowledge artifact.
            output_template = node.get("output", {})
            interp_ctx: Dict[str, Any] = {"state": state, "inputs": params}
            output = interpolation.interpolate(output_template, interp_ctx) if output_template else {}
            return {
                "success": True,
                "output": output,
                "steps": step_count,
                "graph_run_id": graph_run_id,
            }

        # Foreach node — iterate
        if node.get("type") == "foreach":
            graph_transcript.write_event("step_started", {
                "step": step_count, "node": executed_node, "node_type": "foreach",
            })
            foreach_start = time.monotonic()
            current, state = await _handle_foreach(
                node, state, params, exec_ctx, project_path
            )
            graph_transcript.write_event("foreach_completed", {
                "step": step_count, "node": executed_node, "next_node": current,
            })
            foreach_elapsed = time.monotonic() - foreach_start
            _log_progress(graph_id, step_count, len(nodes), executed_node, elapsed_s=foreach_elapsed, status="ok", detail="foreach")
            graph_transcript.checkpoint(step_count, state=state, current_node=current)
            graph_transcript.render_knowledge(
                "running", step_count, time.monotonic() - graph_start_time,
            )
            await _persist_state(
                project_path, graph_id, graph_run_id,
                state, current, "running", step_count,
            )
            if target_node:
                return {
                    "success": True,
                    "state": state,
                    "executed_node": executed_node,
                    "next_node": current,
                    "step_count": step_count,
                }
            continue

        # Build interpolation context
        interp_ctx: Dict[str, Any] = {"state": state, "inputs": params}

        # Gate node — explicit routing/assign, no action execution
        if node.get("type") == "gate":
            graph_transcript.write_event("step_started", {
                "step": step_count, "node": executed_node, "node_type": "gate",
            })
            if "assign" in node:
                for key, expr in node["assign"].items():
                    resolved = interpolation.interpolate(expr, interp_ctx)
                    if resolved is None and expr:
                        logger.warning(
                            "assign '%s' resolved to None for expr '%s'",
                            key, expr,
                        )
                    state[key] = resolved
            next_spec = node.get("next")
            current = _evaluate_edges(next_spec, state, {})
            graph_transcript.write_event("step_completed", {
                "step": step_count, "node": executed_node,
                "action_id": "",
                "status": "ok",
                "elapsed_s": 0,
                "next_node": current,
            })
            _log_progress(graph_id, step_count, len(nodes), executed_node, status="ok", detail="gate")
            graph_transcript.checkpoint(step_count, state=state, current_node=current)
            graph_transcript.render_knowledge(
                "running", step_count, time.monotonic() - graph_start_time,
            )
            await _persist_state(
                project_path, graph_id, graph_run_id,
                state, current, "running", step_count,
            )
            if target_node:
                return {
                    "success": True,
                    "state": state,
                    "executed_node": executed_node,
                    "next_node": current,
                    "step_count": step_count,
                }
            continue

        # Validate action exists on non-typed nodes
        if "action" not in node:
            node_type = node.get("type", "(none)")
            raise KeyError(
                f"Node '{executed_node}' has no 'action' field. "
                f"Nodes must either define 'action', or use an explicit type: "
                f"'return', 'foreach', or 'gate'. Got type={node_type!r}"
            )

        # Interpolate action params from state
        action = interpolation.interpolate_action(node["action"], interp_ctx)
        action["params"] = _strip_none(action.get("params", {}))

        # Inject parent context for thread_directive calls
        if action.get("item_id") == "rye/agent/threads/thread_directive":
            action["params"] = _inject_parent_context(
                action.get("params", {}), exec_ctx
            )

        action_id = action.get("item_id", "")
        graph_transcript.write_event("step_started", {
            "step": step_count, "node": executed_node, "action_id": action_id,
        })
        graph_transcript.render_knowledge(
            "running", step_count, time.monotonic() - graph_start_time,
        )
        node_start = time.monotonic()
        state_keys_before = set(state.keys())
        _log_progress(graph_id, step_count, len(nodes), executed_node)

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
            if error_loader is not None:
                classification = error_loader.classify(
                    Path(project_path), _error_to_context(result)
                )
            else:
                classification = {"retryable": False, "category": "permanent"}
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
                elapsed = time.monotonic() - graph_start_time
                graph_transcript.write_event("graph_error", {
                    "error": result.get("error", "unknown"),
                    "node": executed_node, "steps": step_count,
                    "elapsed_s": elapsed,
                })
                graph_transcript.checkpoint(step_count, state=state, current_node=current)
                graph_transcript.render_knowledge("error", step_count, elapsed)
                await _persist_state(
                    project_path, graph_id, graph_run_id,
                    state, current, "error", step_count,
                )
                if registry is not None:
                    registry.update_status(graph_run_id, "error")
                node_elapsed = time.monotonic() - node_start
                _log_progress(graph_id, step_count, len(nodes), executed_node, elapsed_s=node_elapsed, status="error", detail=str(result.get("error", ""))[:80])
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

        node_elapsed = time.monotonic() - node_start
        graph_transcript.write_event("step_completed", {
            "step": step_count, "node": executed_node,
            "action_id": action_id,
            "status": result.get("status", "ok"),
            "elapsed_s": node_elapsed,
            "next_node": current,
            "thread_id": result.get("thread_id", ""),
            "error": result.get("error", ""),
        })
        added_keys = set(state.keys()) - state_keys_before
        _log_progress(graph_id, step_count, len(nodes), executed_node, elapsed_s=node_elapsed, status="error" if result.get("status") == "error" else "ok", detail=f"+{', '.join(sorted(added_keys))}" if added_keys else "")
        graph_transcript.checkpoint(step_count, state=state, current_node=current)
        graph_transcript.render_knowledge(
            "running", step_count, time.monotonic() - graph_start_time,
        )

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

        # Single-step mode — return after executing one node
        if target_node:
            return {
                "success": result.get("status") != "error",
                "state": state,
                "executed_node": executed_node,
                "next_node": current,
                "step_count": step_count,
            }

        # Cancellation check
        cancel_path = (
            Path(project_path) / AI_DIR / "threads" / graph_run_id / "cancel"
        )
        if cancel_path.exists():
            elapsed = time.monotonic() - graph_start_time
            graph_transcript.write_event("graph_cancelled", {
                "steps": step_count, "elapsed_s": elapsed,
            })
            graph_transcript.checkpoint(step_count, state=state, current_node=current)
            graph_transcript.render_knowledge("cancelled", step_count, elapsed)
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
    elapsed = time.monotonic() - graph_start_time
    graph_transcript.write_event("graph_error", {
        "error": f"max_steps_exceeded ({max_steps})",
        "steps": step_count, "elapsed_s": elapsed,
    })
    graph_transcript.checkpoint(step_count, state=state, current_node=current)
    graph_transcript.render_knowledge("error", step_count, elapsed)
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
    _log_progress(graph_id, step_count, len(nodes), "done", elapsed_s=elapsed, status="error", detail=f"max_steps_exceeded ({max_steps})")
    return {
        "success": False,
        "error": f"Max steps exceeded ({max_steps})",
        "state": state,
    }


# ---------------------------------------------------------------------------
# Param cleaning
# ---------------------------------------------------------------------------


def _strip_none(d: Any) -> Any:
    """Remove None values from nested dicts so tool CONFIG_SCHEMA defaults apply."""
    if isinstance(d, dict):
        return {k: _strip_none(v) for k, v in d.items() if v is not None}
    if isinstance(d, list):
        return [_strip_none(v) for v in d]
    return d


# ---------------------------------------------------------------------------
# Streaming progress (stderr)
# ---------------------------------------------------------------------------

_QUIET = os.environ.get("RYE_GRAPH_QUIET")


def _log_progress(
    graph_id: str,
    step: int,
    total: int,
    node: str,
    *,
    elapsed_s: float = 0,
    status: str = "...",
    detail: str = "",
) -> None:
    """One-line progress to stderr. Set RYE_GRAPH_QUIET=1 to suppress."""
    if _QUIET:
        return
    icons = {"ok": "✓", "error": "✗", "...": "...", "return": "⏹"}
    icon = icons.get(status, status)
    step_str = f"step {step}/{total}" if total else f"step {step}"
    elapsed_str = f" {elapsed_s:.1f}s" if elapsed_s else ""
    detail_str = f" ({detail})" if detail else ""
    sys.stderr.write(
        f"[graph:{graph_id}] {step_str} {node} {icon}{elapsed_str}{detail_str}\n"
    )
    sys.stderr.flush()


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

    Parallel mode: when the node has ``parallel: true``, all iterations are
    dispatched concurrently via asyncio.gather.  Sequential mode (default):
    each iteration completes before the next starts.

    Returns (next_node, updated_state).
    """
    interp_ctx: Dict[str, Any] = {"state": state, "inputs": inputs}
    over_expr = node.get("over", "")
    items = interpolation.interpolate(over_expr, interp_ctx)
    if not isinstance(items, list):
        items = []

    as_var = node.get("as", "item")
    collect_var = node.get("collect")

    # Detect parallel mode from node-level key
    is_parallel = node.get("parallel", False) is True

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
    collected: List[Any] = []

    for item in items:
        state[as_var] = item
        interp_ctx: Dict[str, Any] = {
            "state": state, "inputs": inputs, as_var: item,
        }

        action = interpolation.interpolate_action(node["action"], interp_ctx)
        action["params"] = _strip_none(action.get("params", {}))

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

        if result.get("status") == "error" or result.get("success") is False:
            collected.append(result)
        else:
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

    async def _run_one(item: Any) -> Any:
        interp_ctx: Dict[str, Any] = {
            "state": {"inputs": inputs, as_var: item},
            "inputs": inputs,
            as_var: item,
        }
        action = interpolation.interpolate_action(node["action"], interp_ctx)
        action["params"] = _strip_none(action.get("params", {}))

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

    Supports ``async`` parameter: when True, spawns a child process
    that runs the graph in the background.  The parent returns immediately
    with ``{success, graph_run_id, status: "running"}``.

    Same pattern as thread_directive.py async.
    """
    is_async = params.pop("async", False)

    if is_async:
        thread_registry = _try_load_module("persistence/thread_registry")
        if thread_registry is None:
            return {"success": False, "error": "Agent bundle required for async mode"}

        # Pre-generate graph_run_id so parent can return it
        cfg = graph_config.get("config", {})
        graph_id = graph_config.get("_item_id") or graph_config.get("category", "unknown")
        graph_run_id = f"{graph_id.replace('/', '-')}-{int(time.time())}"

        # Register before subprocess so child process sees it
        parent_thread_id = os.environ.get("RYE_PARENT_THREAD_ID")
        registry = thread_registry.get_registry(Path(project_path))
        registry.register(graph_run_id, graph_id, parent_thread_id)
        registry.update_status(graph_run_id, "running")

        # Prepare log file for child process
        log_dir = Path(project_path) / AI_DIR / "agent" / "graphs" / graph_run_id
        log_dir.mkdir(parents=True, exist_ok=True)
        log_path = log_dir / "spawn.log"

        # Get path to walker.py for __main__ invocation
        walker_path = Path(__file__).resolve()

        # Prepare subprocess arguments
        # The __main__ block will load the graph YAML and call run_sync()
        # We need to add --graph-run-id and --pre-registered to signal
        # the child to call execute() directly instead of run_sync()
        cmd = [
            sys.executable,
            str(walker_path),
            "--graph-path", graph_config.get("_file_path", ""),
            "--params", json.dumps(params),
            "--project-path", project_path,
            "--graph-run-id", graph_run_id,
            "--pre-registered",
        ]

        # Forward env vars so child process can find packages and API keys
        envs = {}
        for key in os.environ:
            if key.startswith(("PYTHON", "RYE_", "USER_SPACE", "ZEN_", "ANTHROPIC_",
                               "OPENAI_", "GOOGLE_", "CONTEXT7_")):
                envs.setdefault(key, os.environ[key])
        for key in ("HOME", "PATH", "LANG", "TERM"):
            if key in os.environ:
                envs.setdefault(key, os.environ[key])

        # Cross-platform detached spawn via orchestrator helper
        orchestrator = _try_load_module("orchestrator")
        if orchestrator is None:
            registry.update_status(graph_run_id, "error")
            return {"success": False, "error": "Agent bundle required for async mode"}
        spawn_result = asyncio.run(orchestrator.spawn_detached(
            cmd=cmd[0],
            args=cmd[1:],
            log_path=str(log_path),
            envs=envs,
        ))

        if not spawn_result.get("success"):
            registry.update_status(graph_run_id, "error")
            return {
                "success": False,
                "error": f"Failed to spawn child process: {spawn_result.get('error')}",
            }

        # Parent — return immediately with child PID
        return {
            "success": True,
            "graph_run_id": graph_run_id,
            "graph_id": graph_id,
            "status": "running",
            "pid": spawn_result["pid"],
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
    parser.add_argument("--graph-run-id", default=None)
    parser.add_argument("--pre-registered", action="store_true")
    args = parser.parse_args()

    if os.environ.get("RYE_DEBUG"):
        logging.basicConfig(
            level=logging.DEBUG,
            format="[%(name)s] %(levelname)s: %(message)s",
            stream=__import__("sys").stderr,
        )

    graph_config = _load_graph_yaml(args.graph_path)
    params = json.loads(args.params)

    # If called from subprocess with --graph-run-id and --pre-registered,
    # call execute() directly (child process behavior)
    try:
        if args.graph_run_id and args.pre_registered:
            result = asyncio.run(execute(
                graph_config,
                params,
                args.project_path,
                graph_run_id=args.graph_run_id,
                pre_registered=True,
            ))
        else:
            # Normal entry (possibly with async=True for fork/subprocess)
            result = run_sync(graph_config, params, args.project_path)
    except Exception as exc:
        result = {"success": False, "error": str(exc)}

    print(json.dumps(result, default=str))
