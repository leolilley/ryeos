# rye:signed:2026-02-23T00:42:51Z:65a92254d121fec01b6d31433ffb6af684749baceb9f04a32238decb35e53daa:eLoufiUVdpWKvix3Gz2U8nszNK5XpGpvg_JcP9NVRzxOfi7CQ2H_jLNpdSUOyHHgRMSKtJT0Qzcl59lXLG82BQ==:9fbfabe975fa5a7f
__version__ = "1.6.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads"
__tool_description__ = "Thread coordination: wait, cancel, status, chain resolution"

from typing import Any, Dict, List, Optional

import asyncio
import os
import signal
from pathlib import Path

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
    """Poll registry for thread completion (cross-process threads)."""
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


async def handoff_thread(
    thread_id: str,
    project_path: Path,
    messages: Optional[List[Dict]] = None,
    continuation_message: Optional[str] = None,
) -> Dict:
    """Handoff a stopping thread to a new continuation thread.

    Builds resume context (trailing messages within token ceiling),
    spawns a new thread with the same directive, and links old→new
    via the continuation chain. Summarization is hook-driven — if the
    directive declares an after_complete hook, it runs in the old thread.

    Args:
        thread_id: The stopping thread to hand off from.
        project_path: Project root path.
        messages: Live messages from runner (None = reconstruct from transcript).
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

    # Load continuation config
    coordination_loader = load_module("loaders/coordination_loader", anchor=_ANCHOR)
    cont_config = coordination_loader.get_coordination_loader().get_continuation_config(project_path)
    resume_ceiling = cont_config.get("resume_ceiling_tokens", 16000)

    # Verify transcript integrity before trusting its content
    from rye.constants import AI_DIR
    transcript_signer_mod = load_module("persistence/transcript_signer", anchor=_ANCHOR)
    signer = transcript_signer_mod.TranscriptSigner(
        thread_id, project_path / AI_DIR / "agent" / "threads" / thread_id
    )
    integrity_policy = cont_config.get("transcript_integrity", "strict")
    integrity = signer.verify(allow_unsigned_trailing=(integrity_policy == "lenient"))
    if not integrity["valid"]:
        return {
            "success": False,
            "error": f"Transcript integrity check failed: {integrity['error']}. "
                     f"Cannot hand off from tampered transcript.",
        }

    # Reconstruct messages from transcript if not provided live
    if messages is None:
        transcript_mod = load_module("persistence/transcript", anchor=_ANCHOR)
        transcript_obj = transcript_mod.Transcript(thread_id, project_path)
        messages = transcript_obj.reconstruct_messages()
        if not messages:
            return {"success": False, "error": f"Cannot reconstruct messages for thread: {thread_id}"}

    # --- Phase 1: Fill ceiling budget with trailing messages ---
    trailing_messages: List[Dict] = []
    trailing_tokens = 0
    for msg in reversed(messages):
        msg_tokens = len(str(msg.get("content", ""))) // 4
        if trailing_tokens + msg_tokens > resume_ceiling:
            break
        trailing_messages.insert(0, msg)
        trailing_tokens += msg_tokens

    if not trailing_messages and messages:
        trailing_messages = [messages[-1]]

    # Ensure trailing slice starts with a user message (providers require it)
    while trailing_messages and trailing_messages[0].get("role") not in ("user",):
        trailing_messages.pop(0)

    # --- Phase 2: Build resume_messages ---
    resume_messages: List[Dict] = []
    resume_messages.extend(trailing_messages)

    if continuation_message:
        resume_messages.append({"role": "user", "content": continuation_message})
    else:
        resume_messages.append({
            "role": "user",
            "content": "Continue executing the directive. Pick up where the previous thread left off.",
        })

    # --- Phase 3: Spawn new thread via thread_directive ---
    # Treat continuation as a normal spawn from the same parent.
    # Same parent chain, same spawn/depth checks, same limits resolution.
    thread_directive_mod = load_module("thread_directive", anchor=_ANCHOR)
    parent_id = thread.get("parent_id")
    spawn_params = {
        "directive_id": directive_name,
        "resume_messages": resume_messages,
        "previous_thread_id": thread_id,
    }
    if parent_id:
        spawn_params["parent_thread_id"] = parent_id

    new_result = await thread_directive_mod.execute(spawn_params, str(project_path))

    new_thread_id = new_result.get("thread_id")

    # --- Phase 4: Link old → new in registry ---
    if new_thread_id:
        registry.set_continuation(thread_id, new_thread_id)
        chain = registry.get_chain(thread_id)
        chain_root_id = chain[0]["thread_id"] if chain else thread_id
        registry.set_chain_info(new_thread_id, chain_root_id, thread_id)

    # --- Phase 5: Log handoff in old thread's transcript ---
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
            "trailing_turns": len(trailing_messages),
        },
        old_transcript,
        criticality="critical",
    )

    return {
        "success": new_result.get("success", False),
        "old_thread_id": thread_id,
        "new_thread_id": new_thread_id,
        "directive": directive_name,
        "trailing_turns": len(trailing_messages),
        "resume_ceiling_tokens": resume_ceiling,
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
        try:
            os.kill(pid, signal.SIGTERM)
            # Wait briefly for graceful shutdown
            import time
            for _ in range(10):
                time.sleep(0.3)
                try:
                    os.kill(pid, 0)  # Check if still alive
                except OSError:
                    break  # Process is gone
            else:
                # Still alive after 3s — force kill
                try:
                    os.kill(pid, signal.SIGKILL)
                except OSError:
                    pass
        except OSError as e:
            if e.errno == 3:  # ESRCH — no such process (already dead)
                pass
            else:
                return {"success": False, "error": f"Failed to kill PID {pid}: {e}"}
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

        # Verify transcript integrity before trusting its content
        from rye.constants import AI_DIR
        transcript_signer_mod = load_module("persistence/transcript_signer", anchor=_ANCHOR)
        signer = transcript_signer_mod.TranscriptSigner(
            resolved_id, proj_path / AI_DIR / "agent" / "threads" / resolved_id
        )
        coordination_loader = load_module("loaders/coordination_loader", anchor=_ANCHOR)
        cont_config = coordination_loader.get_coordination_loader().get_continuation_config(proj_path)
        integrity_policy = cont_config.get("transcript_integrity", "strict")
        integrity = signer.verify(allow_unsigned_trailing=(integrity_policy == "lenient"))
        if not integrity["valid"]:
            return {
                "success": False,
                "error": f"Transcript integrity check failed: {integrity['error']}. "
                         f"Cannot resume from tampered transcript.",
            }

        # Reconstruct full conversation from transcript
        transcript_mod = load_module("persistence/transcript", anchor=_ANCHOR)
        transcript_obj = transcript_mod.Transcript(resolved_id, proj_path)
        existing_messages = transcript_obj.reconstruct_messages()
        if not existing_messages:
            return {"success": False, "error": f"Cannot reconstruct messages for thread: {resolved_id}"}

        # Full reconstruction + new message. If it's too big for context,
        # the runner's context_limit_reached will trigger handoff_thread.
        resume_messages = list(existing_messages)
        resume_messages.append({"role": "user", "content": message})

        # Spawn as sibling of the original (same parent, same guards)
        thread_directive_mod = load_module("thread_directive", anchor=_ANCHOR)
        parent_id = thread.get("parent_id")
        spawn_params = {
            "directive_id": directive_name,
            "resume_messages": resume_messages,
            "previous_thread_id": resolved_id,
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
                "reconstructed_turns": len(existing_messages),
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
            "reconstructed_turns": len(existing_messages),
            "new_thread_result": new_result,
        }

    if operation == "handoff_thread":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        result = await handoff_thread(thread_id, proj_path)
        return result

    return {"success": False, "error": f"Unknown operation: {operation}"}
