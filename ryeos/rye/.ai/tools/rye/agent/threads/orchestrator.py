# rye:signed:2026-02-25T00:02:14Z:2df9efb78016a4bd0661fc756d316ee7732fef4ae71d80a7339e82ddb32bf7fe:lU8D_itr7-mmBW__m4Qy1ua4ezc8owMxO-wREV4ZijaREUB8nRMaCYmzjO41fIAp_Lk33_Qn1pMmWkoPAB00CA==:9fbfabe975fa5a7f
__version__ = "1.6.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads"
__tool_description__ = "Thread coordination: wait, cancel, status, chain resolution"

from typing import Any, Dict, List, Optional

import asyncio
import json as _json
import shutil
from pathlib import Path

from lilux.primitives.subprocess import SubprocessPrimitive
from module_loader import load_module

_ANCHOR = Path(__file__).parent


CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {
            "type": "string",
            "enum": [
                "wait_threads", "cancel_thread", "kill_thread",
                "get_status", "list_active", "aggregate_results",
                "get_chain", "chain_search",
                "read_transcript", "resume_thread", "handoff_thread",
            ],
        },
        "thread_ids": {"type": "array", "items": {"type": "string"}},
        "thread_id": {"type": "string"},
        "timeout": {"type": "number"},
        "query": {"type": "string", "description": "Search query for chain_search"},
        "search_type": {"type": "string", "enum": ["regex", "text"], "default": "text"},
        "max_results": {"type": "integer", "default": 50},
        "tail_lines": {"type": "integer", "description": "Number of lines from end of transcript to return"},
        "message": {"type": "string", "description": "User message to append for resume_thread"},
    },
    "required": ["operation"],
}


# In-process tracking (only valid within the same Python process)
_thread_events: Dict[str, asyncio.Event] = {}
_thread_results: Dict[str, Dict] = {}
_active_harnesses: Dict[str, Any] = {}
_spawn_counts: Dict[str, int] = {}
_thread_depths: Dict[str, int] = {}


def register_thread(thread_id: str, harness: Any, depth: int = 0) -> None:
    """Called by runner.py at thread start. Creates Event for wait coordination."""
    _thread_events[thread_id] = asyncio.Event()
    _active_harnesses[thread_id] = harness
    _thread_depths[thread_id] = depth


def get_depth(thread_id: str) -> int:
    """Get current depth of a thread. 0 = root."""
    return _thread_depths.get(thread_id, 0)


def check_spawn_limit(parent_thread_id: str, limit: int) -> Optional[Dict]:
    """Check if parent has exceeded spawn limit. Returns error dict or None."""
    count = _spawn_counts.get(parent_thread_id, 0)
    if count >= limit:
        return {
            "limit_code": "spawns_exceeded",
            "current_value": count,
            "current_max": limit,
        }
    return None


def increment_spawn_count(parent_thread_id: str) -> int:
    """Increment spawn count for parent. Returns new count."""
    _spawn_counts[parent_thread_id] = _spawn_counts.get(parent_thread_id, 0) + 1
    return _spawn_counts[parent_thread_id]


def complete_thread(thread_id: str, result: Dict) -> None:
    """Called by runner.py in finally block. Signals Event so wait_threads unblocks."""
    _thread_results[thread_id] = result
    event = _thread_events.get(thread_id)
    if event:
        event.set()
    _active_harnesses.pop(thread_id, None)
    _spawn_counts.pop(thread_id, None)


def resolve_thread_chain(thread_id: str, project_path: Path) -> str:
    """Follow continuation chain to terminal thread.

    Returns the thread_id of the terminal thread (completed/error/running).
    If the thread is not continued or not in registry, returns the original id.
    """
    thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
    registry = thread_registry.get_registry(project_path)

    current = thread_id
    visited = set()

    while True:
        if current in visited:
            return current  # cycle — stop
        visited.add(current)

        thread = registry.get_thread(current)
        if not thread:
            return current

        if thread.get("status") != "continued":
            return current

        continuation_id = thread.get("continuation_thread_id")
        if not continuation_id:
            return current
        current = continuation_id


async def _wait_single(thread_id: str, timeout: float, project_path: Path) -> Dict:
    """Wait for a single thread, resolving continuation chains."""
    # Resolve chain first
    resolved_id = resolve_thread_chain(thread_id, project_path)

    # Check if already completed in-process
    if resolved_id in _thread_results:
        return _thread_results[resolved_id]

    # Wait on in-process event
    event = _thread_events.get(resolved_id)
    if event:
        try:
            await asyncio.wait_for(event.wait(), timeout=timeout)
            return _thread_results.get(resolved_id, {"status": "unknown"})
        except asyncio.TimeoutError:
            return {"status": "timeout", "thread_id": resolved_id}

    # Not in-process — check registry for final status
    thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
    registry = thread_registry.get_registry(project_path)
    thread = registry.get_thread(resolved_id)
    if thread:
        status = thread.get("status", "unknown")
        if status in ("completed", "error", "cancelled", "continued"):
            return {"status": status, "thread_id": resolved_id}
        # Still running but not in our process — poll registry
        return await _poll_registry(resolved_id, registry, timeout)

    return {"status": "not_found", "thread_id": resolved_id}


async def _poll_registry(thread_id: str, registry, timeout: float) -> Dict:
    """Wait for thread completion using lilux-watch (push) or polling (fallback)."""
    # Try push-based watcher first
    lilux_watch = shutil.which("lilux-watch")
    if lilux_watch and hasattr(registry, "db_path"):
        try:
            proc = await asyncio.create_subprocess_exec(
                lilux_watch,
                "--db", str(registry.db_path),
                "--thread-id", thread_id,
                "--timeout", str(timeout),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=timeout + 5)
            if proc.returncode == 0 and stdout:
                return _json.loads(stdout.strip())
        except (asyncio.TimeoutError, OSError, ValueError):
            pass  # fall through to polling

    # Fallback: 500ms polling
    import time
    deadline = time.monotonic() + timeout
    poll_interval = 0.5

    while time.monotonic() < deadline:
        thread = registry.get_thread(thread_id)
        if thread:
            status = thread.get("status", "unknown")
            if status in ("completed", "error", "cancelled", "continued"):
                return {"status": status, "thread_id": thread_id}
        await asyncio.sleep(min(poll_interval, deadline - time.monotonic()))

    return {"status": "timeout", "thread_id": thread_id}


_subprocess = SubprocessPrimitive()


async def _kill_pid(pid: int, grace: float = 3.0) -> Dict:
    """Kill a process by PID via SubprocessPrimitive."""
    result = await _subprocess.kill(pid, grace=grace)
    return {"success": result.success, "pid": pid, "method": result.method, "error": result.error}


async def spawn_detached(
    cmd: str, args: List[str],
    log_path: Optional[str] = None,
    envs: Optional[Dict[str, str]] = None,
) -> Dict:
    """Spawn a detached process via SubprocessPrimitive.

    Returns dict with 'success' and 'pid' on success.
    """
    result = await _subprocess.spawn(cmd, args, log_path=log_path, envs=envs)
    if result.success:
        return {"success": True, "pid": result.pid}
    return {"success": False, "error": result.error}


async def handoff_thread(
    thread_id: str,
    project_path: Path,
    messages: Optional[List[Dict]] = None,
    continuation_message: Optional[str] = None,
) -> Dict:
    """Handoff a stopping thread to a new continuation thread.

    Spawns a new thread with the same directive and links old→new
    via the continuation chain. The new thread reconstructs resume
    context from the previous thread's transcript JSONL (verified
    for integrity in execute() step 3.5).

    Args:
        thread_id: The stopping thread to hand off from.
        project_path: Project root path.
        messages: Ignored (kept for API compat). Resume is from JSONL.
        continuation_message: Optional user message to append (for resume_thread).

    Returns:
        Dict with new_thread_id, success, and handoff metadata.
    """
    thread_registry_mod = load_module("persistence/thread_registry", anchor=_ANCHOR)
    registry = thread_registry_mod.get_registry(project_path)

    thread = registry.get_thread(thread_id)
    if not thread:
        return {"success": False, "error": f"Thread not found: {thread_id}"}

    directive_name = thread.get("directive")
    if not directive_name:
        return {"success": False, "error": "No directive recorded for thread"}

    # Spawn new thread — execute() step 3.5 handles transcript integrity
    # verification, JSONL reconstruction, and ceiling trimming.
    thread_directive_mod = load_module("thread_directive", anchor=_ANCHOR)
    parent_id = thread.get("parent_id")
    spawn_params = {
        "directive_id": directive_name,
        "previous_thread_id": thread_id,
    }
    if continuation_message:
        spawn_params["_continuation_message"] = continuation_message
    if parent_id:
        spawn_params["parent_thread_id"] = parent_id

    new_result = await thread_directive_mod.execute(spawn_params, str(project_path))

    new_thread_id = new_result.get("thread_id")

    # Link old → new in registry
    if new_thread_id:
        registry.set_continuation(thread_id, new_thread_id)
        chain = registry.get_chain(thread_id)
        chain_root_id = chain[0]["thread_id"] if chain else thread_id
        registry.set_chain_info(new_thread_id, chain_root_id, thread_id)

    # Log handoff in old thread's transcript
    EventEmitter = load_module("events/event_emitter", anchor=_ANCHOR).EventEmitter
    Transcript = load_module("persistence/transcript", anchor=_ANCHOR).Transcript
    emitter = EventEmitter(project_path)
    old_transcript = Transcript(thread_id, project_path)
    emitter.emit(
        thread_id,
        "thread_handoff",
        {
            "new_thread_id": new_thread_id,
            "directive": directive_name,
        },
        old_transcript,
        criticality="critical",
    )

    return {
        "success": new_result.get("success", False),
        "old_thread_id": thread_id,
        "new_thread_id": new_thread_id,
        "directive": directive_name,
        "new_thread_result": new_result,
    }


async def execute(params: Dict, project_path: str) -> Dict:
    operation = params["operation"]
    proj_path = Path(project_path)

    if operation == "wait_threads":
        thread_ids = params.get("thread_ids", [])
        timeout = params.get("timeout")

        if timeout is None:
            try:
                coordination_loader = load_module("loaders/coordination_loader", anchor=_ANCHOR)
                config = coordination_loader.load(proj_path)
                timeout = config.get("coordination", {}).get("wait_threads", {}).get("default_timeout", 600.0)
            except Exception:
                timeout = 600.0

        # Wait for all threads concurrently
        wait_tasks = [
            _wait_single(tid, timeout, proj_path)
            for tid in thread_ids
        ]
        results_list = await asyncio.gather(*wait_tasks, return_exceptions=True)

        results = {}
        for i, (tid, result) in enumerate(zip(thread_ids, results_list)):
            # tid may be a dict (failed spawn error) instead of a string
            key = tid if isinstance(tid, str) else f"__failed_{i}"
            if isinstance(result, Exception):
                results[key] = {"status": "error", "error": str(result)}
            elif not isinstance(tid, str):
                # Spawn itself failed — tid is the error dict
                results[key] = tid if isinstance(tid, dict) else {"status": "error", "error": str(tid)}
            else:
                results[key] = result

        all_success = all(
            r.get("status") == "completed"
            for r in results.values()
        )
        return {"success": all_success, "results": results}

    if operation == "cancel_thread":
        thread_id = params.get("thread_id")
        harness = _active_harnesses.get(thread_id)
        if harness:
            harness.request_cancel()
            return {"success": True, "cancelled": thread_id}
        return {"success": False, "error": f"Thread not found: {thread_id}"}

    if operation == "kill_thread":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
        registry = thread_registry.get_registry(proj_path)
        thread = registry.get_thread(thread_id)
        if not thread:
            return {"success": False, "error": f"Thread not found: {thread_id}"}
        pid = thread.get("pid")
        if not pid:
            return {"success": False, "error": f"No PID recorded for thread: {thread_id}"}
        kill_result = await _kill_pid(pid)
        if not kill_result.get("success"):
            return {"success": False, "error": f"Failed to kill PID {pid}: {kill_result.get('error')}"}
        registry.update_status(thread_id, "killed")
        # Clean up in-process tracking if it exists
        _active_harnesses.pop(thread_id, None)
        event = _thread_events.get(thread_id)
        if event:
            event.set()
        return {"success": True, "killed": thread_id, "pid": pid}

    if operation == "get_status":
        thread_id = params.get("thread_id")
        # In-process check first
        if thread_id in _thread_results:
            return {"success": True, **_thread_results[thread_id]}
        if thread_id in _thread_events:
            return {"success": True, "status": "running"}
        # Registry fallback
        thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
        registry = thread_registry.get_registry(proj_path)
        thread = registry.get_thread(thread_id)
        if thread:
            return {"success": True, "status": thread.get("status"), "thread_id": thread_id}
        return {"success": False, "error": f"Thread not found: {thread_id}"}

    if operation == "list_active":
        active = [tid for tid, event in _thread_events.items() if not event.is_set()]
        return {"success": True, "active_threads": active, "count": len(active)}

    if operation == "aggregate_results":
        thread_ids = params.get("thread_ids", [])
        results = {}
        for tid in thread_ids:
            if tid in _thread_results:
                results[tid] = _thread_results[tid]
            else:
                thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
                registry = thread_registry.get_registry(proj_path)
                thread = registry.get_thread(tid)
                if thread:
                    results[tid] = {"status": thread.get("status"), "thread_id": tid}
                else:
                    results[tid] = {"status": "not_found"}
        return {"success": True, "results": results}

    if operation == "get_chain":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
        registry = thread_registry.get_registry(proj_path)
        chain = registry.get_chain(thread_id)
        return {
            "success": True,
            "chain_length": len(chain),
            "chain": [
                {"thread_id": t["thread_id"], "status": t.get("status"), "directive": t.get("directive")}
                for t in chain
            ],
        }

    if operation == "chain_search":
        thread_id = params.get("thread_id")
        query = params.get("query")
        if not thread_id or not query:
            return {"success": False, "error": "thread_id and query required"}
        chain_search = load_module("internal/thread_chain_search", anchor=_ANCHOR)
        return chain_search.execute({
            "thread_id": thread_id,
            "query": query,
            "search_type": params.get("search_type", "text"),
            "max_results": params.get("max_results", 50),
        }, project_path)

    if operation == "read_transcript":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        from rye.constants import AI_DIR
        from pathlib import PurePosixPath
        thread_path = PurePosixPath(thread_id)
        transcript_path = proj_path / AI_DIR / "knowledge" / "agent" / "threads" / thread_path.parent / f"{thread_path.name}.md"
        if not transcript_path.exists():
            return {"success": False, "error": f"Transcript not found for thread: {thread_id}"}
        content = transcript_path.read_text(encoding="utf-8")
        tail_lines = params.get("tail_lines")
        if tail_lines and tail_lines > 0:
            lines = content.splitlines()
            content = "\n".join(lines[-tail_lines:])
        return {"success": True, "thread_id": thread_id, "content": content}

    if operation == "resume_thread":
        thread_id = params.get("thread_id")
        message = params.get("message")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        if not message:
            return {"success": False, "error": "message required"}
        thread_registry_local = load_module("persistence/thread_registry", anchor=_ANCHOR)
        registry = thread_registry_local.get_registry(proj_path)

        resolved_id = resolve_thread_chain(thread_id, proj_path)
        thread = registry.get_thread(resolved_id)
        if not thread:
            return {"success": False, "error": f"Thread not found: {resolved_id}"}
        status = thread.get("status")
        if status in ("running", "created"):
            return {"success": False, "error": f"Thread is still {status}, cannot resume"}

        directive_name = thread.get("directive")
        if not directive_name:
            return {"success": False, "error": "No directive recorded for thread"}

        # Spawn new thread — execute() step 3.5 handles transcript integrity
        # verification, JSONL reconstruction, and ceiling trimming.
        thread_directive_mod = load_module("thread_directive", anchor=_ANCHOR)
        parent_id = thread.get("parent_id")
        spawn_params = {
            "directive_id": directive_name,
            "previous_thread_id": resolved_id,
            "_continuation_message": message,
        }
        if parent_id:
            spawn_params["parent_thread_id"] = parent_id

        new_result = await thread_directive_mod.execute(spawn_params, str(proj_path))

        new_thread_id = new_result.get("thread_id")

        # Link old → new
        if new_thread_id:
            registry.set_continuation(resolved_id, new_thread_id)
            chain = registry.get_chain(resolved_id)
            chain_root_id = chain[0]["thread_id"] if chain else resolved_id
            registry.set_chain_info(new_thread_id, chain_root_id, resolved_id)

        # Log in old transcript
        EventEmitter = load_module("events/event_emitter", anchor=_ANCHOR).EventEmitter
        Transcript = load_module("persistence/transcript", anchor=_ANCHOR).Transcript
        emitter = EventEmitter(proj_path)
        old_transcript = Transcript(resolved_id, proj_path)
        emitter.emit(
            resolved_id,
            "thread_resumed",
            {
                "new_thread_id": new_thread_id,
                "directive": directive_name,
                "message_preview": message[:200],
            },
            old_transcript,
            criticality="critical",
        )

        return {
            "success": new_result.get("success", False),
            "resumed": True,
            "old_thread_id": resolved_id,
            "new_thread_id": new_thread_id,
            "original_thread_id": thread_id if thread_id != resolved_id else None,
            "resolved_thread_id": resolved_id,
            "directive": directive_name,
            "new_thread_result": new_result,
        }

    if operation == "handoff_thread":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        result = await handoff_thread(thread_id, proj_path)
        return result

    return {"success": False, "error": f"Unknown operation: {operation}"}
