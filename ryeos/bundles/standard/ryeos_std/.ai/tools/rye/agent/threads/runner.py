# rye:signed:2026-02-26T06:42:42Z:e47e07a696f6c3bc8ff7b37a1ce2956c5c4fdd54c2b01629a9544e9625dcac05:ro6I4jf3n1LYxnKFQHEAWj7ZfrUSg-5mf84qyZUUzEFKnP0R0Mj38EYhDOz95WlPstNfLXy-T1cf53S1ocMLBw==:4b987fd4e40303ac
"""
runner.py: Core LLM loop for thread execution

Main loop that:
1. Calls LLM with current prompt
2. Parses LLM response for tool calls
3. Executes tool calls via ToolDispatcher
4. Evaluates hooks
5. Checks limits
6. Repeats until completion or error
"""

__version__ = "1.9.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads"
__tool_description__ = "Core LLM loop for thread execution"

import asyncio
import logging
import os
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from module_loader import load_module

logger = logging.getLogger(__name__)

_ANCHOR = Path(__file__).parent

orchestrator = load_module("orchestrator", anchor=_ANCHOR)
text_tool_parser = load_module("internal/text_tool_parser", anchor=_ANCHOR)
thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
tool_result_guard = load_module("internal/tool_result_guard", anchor=_ANCHOR)


async def run(
    thread_id: str,
    user_prompt: str,
    harness: "SafetyHarness",
    provider: "ProviderAdapter",
    dispatcher: "ToolDispatcher",
    emitter: "EventEmitter",
    transcript: Any,
    project_path: Path,
    resume_messages: Optional[List[Dict]] = None,
    directive_body: str = "",
    previous_thread_id: Optional[str] = None,
    inputs: Optional[Dict] = None,
    system_prompt: str = "",
    directive_context: Optional[Dict] = None,
) -> Dict:
    """Execute the LLM loop until completion, error, or limit.

    System prompt is assembled from build_system_prompt hooks (identity,
    behavior, tool protocol) and passed to the provider for each LLM call.
    Tools are passed via API tool definitions.
    Context framing injected via thread_started hooks into the first user message.

    First message construction:
      1. run_hooks_context() dispatches thread_started hooks
      2. Each hook loads a knowledge/tool item, content is extracted
      3. Hook context + user_prompt assembled into a single user message

    Each turn:
      1. Check limits (pre-turn)
      2. Send messages to LLM via provider
      3. Parse response for tool calls
      4. Execute tool calls via dispatcher
      5. Run hooks (after_step)
      6. Check cancellation

    If resume_messages is provided, skips first message construction
    and uses the pre-built messages directly (for thread resumption).
    """
    thread_ctx = {"emitter": emitter, "transcript": transcript, "thread_id": thread_id}

    orchestrator.register_thread(thread_id, harness)

    # Build name → item_id lookup from tool schemas
    tool_id_map = {
        t["name"]: t["_item_id"]
        for t in harness.available_tools
        if "_item_id" in t
    }

    messages = []
    cost = {"turns": 0, "input_tokens": 0, "output_tokens": 0, "spend": 0.0}

    start_time = time.monotonic()

    # Checkpoint signing for transcript integrity
    transcript_signer_mod = load_module("persistence/transcript_signer", anchor=_ANCHOR)
    signer = transcript_signer_mod.TranscriptSigner(
        thread_id, project_path / ".ai" / "agent" / "threads" / thread_id
    )

    # Directive-level context suppressions (e.g. <suppress>tool-protocol</suppress>)
    suppress = (directive_context or {}).get("suppress", [])

    # Assemble system prompt from build_system_prompt hooks + caller override
    system_ctx = await harness.run_hooks_context(
        {
            "directive": harness.directive_name,
            "directive_body": directive_body,
            "model": provider.model,
            "limits": harness.limits,
            "inputs": inputs or {},
        },
        dispatcher,
        event="build_system_prompt",
        suppress=suppress,
    )
    hook_system = "\n\n".join(filter(None, [system_ctx["before"], system_ctx["after"]]))
    if hook_system and system_prompt:
        system_prompt = hook_system + "\n\n" + system_prompt
    elif hook_system:
        system_prompt = hook_system

    if system_prompt:
        emitter.emit(
            thread_id,
            "system_prompt",
            {
                "text": system_prompt,
                "layers": [b["id"] for b in system_ctx.get("before_raw", []) + system_ctx.get("after_raw", [])],
            },
            transcript,
        )

    try:
        if resume_messages:
            # Continuation mode: fire thread_continued hooks
            messages = list(resume_messages)
            hook_ctx = await harness.run_hooks_context(
                {
                    "directive": harness.directive_name,
                    "directive_body": directive_body,
                    "model": provider.model,
                    "limits": harness.limits,
                    "previous_thread_id": previous_thread_id,
                    "inputs": inputs or {},
                },
                dispatcher,
                event="thread_continued",
                suppress=suppress,
            )
            combined = "\n\n".join(filter(None, [hook_ctx["before"], hook_ctx["after"]]))
            if combined and messages:
                # Inject context near the last user message, not at position 0.
                # insert(0) would disrupt the reconstructed conversation chronology
                # and push context far from the continuation ask.
                last_user_idx = len(messages) - 1
                for i in range(len(messages) - 1, -1, -1):
                    if messages[i].get("role") == "user":
                        last_user_idx = i
                        break
                messages[last_user_idx]["content"] = (
                    combined + "\n\n" + messages[last_user_idx]["content"]
                )
            _emit_context_injected(hook_ctx, emitter, thread_id, transcript)
        else:
            # Fresh thread: fire thread_started hooks (identity, rules, knowledge)
            depth = orchestrator.get_depth(thread_id)
            caps = harness._capabilities
            hook_ctx = await harness.run_hooks_context(
                {
                    "directive": harness.directive_name,
                    "directive_body": directive_body,
                    "model": provider.model,
                    "limits": harness.limits,
                    "inputs": inputs or {},
                    "project_path": str(project_path),
                    "depth": depth,
                    "parent_thread_id": previous_thread_id or "none",
                    "spend_limit": harness.limits.get("spend", "unlimited"),
                    "max_turns": harness.limits.get("turns", "unlimited"),
                    "capabilities_summary": ", ".join(caps) if caps else "unrestricted",
                },
                dispatcher,
                event="thread_started",
                suppress=suppress,
            )

            # Merge hook context with directive-level before/after content.
            # Order: hook_before → directive_before → prompt → directive_after → hook_after
            dir_before = (directive_context or {}).get("before", "")
            dir_after = (directive_context or {}).get("after", "")

            first_message_parts = []
            if hook_ctx["before"]:
                first_message_parts.append(hook_ctx["before"])
            if dir_before:
                first_message_parts.append(dir_before)
            first_message_parts.append(user_prompt)
            if dir_after:
                first_message_parts.append(dir_after)
            if hook_ctx["after"]:
                first_message_parts.append(hook_ctx["after"])
            messages.append({"role": "user", "content": "\n\n".join(first_message_parts)})
            _emit_context_injected(hook_ctx, emitter, thread_id, transcript)

        while True:
            # Pre-turn limit check
            cost["elapsed_seconds"] = time.monotonic() - start_time
            limit_result = harness.check_limits(cost)
            if limit_result:
                # Capture error context from recent conversation
                limit_result["error_context"] = _extract_error_context(messages)

                hook_result = await harness.run_hooks(
                    "limit", limit_result, dispatcher, thread_ctx
                )
                if hook_result:
                    hook_result.setdefault("error_context", limit_result["error_context"])
                    return _finalize(
                        thread_id, cost, hook_result, emitter, transcript, signer
                    )
                # Fail-safe: terminate even if no hook handled the limit
                limit_code = limit_result.get("limit_code", "unknown_limit")
                current = limit_result.get("current_value", "?")
                maximum = limit_result.get("current_max", "?")
                return _finalize(
                    thread_id,
                    cost,
                    {
                        "success": False,
                        "error": f"Limit exceeded: {limit_code} ({current}/{maximum})",
                        "error_context": limit_result["error_context"],
                    },
                    emitter,
                    transcript,
                    signer,
                )

            # Cancellation check
            if harness.is_cancelled():
                return _finalize(
                    thread_id,
                    cost,
                    {"success": False, "status": "cancelled"},
                    emitter,
                    transcript,
                    signer,
                )

            # Checkpoint: sign previous turn and update knowledge entry
            if cost["turns"] > 0:
                signer.checkpoint(cost["turns"])
                transcript.render_knowledge_transcript(
                    directive=harness.directive_name,
                    status="running",
                    model=provider.model,
                    cost=cost,
                )

            # LLM call
            cost["turns"] += 1
            emitter.emit(
                thread_id,
                "cognition_in",
                {"text": messages[-1]["content"], "role": messages[-1]["role"]},
                transcript,
            )

            try:
                if provider.supports_streaming:
                    from .events.transcript_sink import TranscriptSink
                    stream_sink = TranscriptSink(
                        transcript_path=transcript._path,
                        thread_id=thread_id,
                        response_format=getattr(provider, "_response_format", "content_blocks"),
                        knowledge_path=transcript.knowledge_path,
                        turn=cost["turns"],
                    )
                    response = await provider.create_streaming_completion(
                        messages, harness.available_tools, sinks=[stream_sink],
                        system_prompt=system_prompt,
                    )
                else:
                    response = await provider.create_completion(
                        messages, harness.available_tools,
                        system_prompt=system_prompt,
                    )
            except Exception as e:
                if os.environ.get("RYE_DEBUG"):
                    import traceback
                    logger.error("LLM call failed: %s: %s\n%s", type(e).__name__, e, traceback.format_exc())

                original_error = {"success": False, "error": str(e) or type(e).__name__}

                error_loader = load_module("loaders/error_loader", anchor=_ANCHOR)
                classification = error_loader.classify(
                    project_path, _error_to_context(e)
                )
                hook_result = await harness.run_hooks(
                    "error",
                    {"error": e, "classification": classification},
                    dispatcher,
                    thread_ctx,
                )
                if hook_result:
                    if hook_result.get("action") == "retry":
                        delay = error_loader.get_error_loader().calculate_retry_delay(
                            project_path,
                            classification.get("retry_policy", {}),
                            cost["turns"],
                        )
                        await asyncio.sleep(delay)
                        continue
                    # Preserve original error if hook blanked it out
                    if not hook_result.get("error"):
                        hook_result["error"] = original_error["error"]
                    if "success" not in hook_result:
                        hook_result["success"] = False
                    return _finalize(
                        thread_id, cost, hook_result, emitter, transcript, signer
                    )
                return _finalize(
                    thread_id, cost, original_error, emitter, transcript, signer
                )

            # Track tokens
            cost["input_tokens"] += response.get("input_tokens", 0)
            cost["output_tokens"] += response.get("output_tokens", 0)
            cost["spend"] += response.get("spend", 0.0)

            emitter.emit(
                thread_id,
                "cognition_out",
                {"text": response["text"], "model": provider.model},
                transcript,
            )

            if response.get("thinking"):
                emitter.emit_critical(
                    thread_id,
                    "cognition_reasoning",
                    {"text": response["thinking"]},
                    transcript,
                )

            # Process tool calls based on provider's tool_use mode
            tool_calls = response.get("tool_calls", [])
            text_parsed = False

            if not tool_calls and provider.tool_use_mode == "text_parsed":
                # Provider doesn't support native tool_use — parse from text
                tool_calls = text_tool_parser.extract_tool_calls(
                    response.get("text", "")
                )
                text_parsed = bool(tool_calls)

            if not tool_calls:
                # Detect empty/stalled responses: if the LLM returned no text
                # AND no tool calls, it likely stalled (common with Gemini).
                # Also nudge if the directive expects structured outputs via
                # directive_return but none was called yet.
                empty_response = not response["text"].strip()
                expects_return = bool(getattr(harness, "output_fields", None))
                nudge_count = getattr(harness, "_nudge_count", 0)
                max_nudges = 3

                should_nudge = (
                    provider.tool_use_mode == "native"
                    and harness.available_tools
                    and nudge_count < max_nudges
                    and (
                        cost["turns"] == 1  # first turn, never produced tools
                        or empty_response   # stalled: empty text + no tools
                        or expects_return   # directive expects directive_return
                    )
                )

                if should_nudge:
                    harness._nudge_count = nudge_count + 1
                    msg = {"role": "assistant", "content": response["text"] or ""}
                    if response.get("thinking"):
                        msg["_thinking"] = response["thinking"]
                    messages.append(msg)
                    if empty_response:
                        nudge_text = (
                            "Your response was empty. You MUST continue working on the directive. "
                            "Use the provided tools to complete all steps. Do not stop until you "
                            "have written the required files and called directive_return."
                        )
                    elif expects_return:
                        nudge_text = (
                            "You have not yet called directive_return. The directive requires "
                            "structured outputs. Continue working: use tools to complete all steps, "
                            "then call rye_execute with item_id='rye/agent/threads/directive_return' "
                            "to return your results."
                        )
                    else:
                        nudge_text = (
                            "You did not call any tools. Please use the provided tools to "
                            "complete the directive steps. Call tools using the tool_use mechanism."
                        )
                    messages.append({"role": "user", "content": nudge_text})
                    continue

                completion_result = {"success": True, "result": response["text"]}
                return _finalize(
                    thread_id,
                    cost,
                    completion_result,
                    emitter,
                    transcript,
                    signer,
                )

            # Append assistant message to conversation
            assistant_msg = {"role": "assistant", "content": response["text"]}
            if response.get("thinking"):
                assistant_msg["_thinking"] = response["thinking"]
            if text_parsed:
                # Text-parsed: assistant message is plain text (no tool_use blocks)
                messages.append(assistant_msg)
            else:
                # API structured: include tool_use blocks for provider reconstruction
                assistant_msg["tool_calls"] = tool_calls
                messages.append(assistant_msg)

            for tool_call in tool_calls:
                emitter.emit(
                    thread_id,
                    "tool_call_start",
                    {
                        "tool": tool_call["name"],
                        "call_id": tool_call["id"],
                        "input": tool_call["input"],
                    },
                    transcript,
                )

                # Permission check: extract the inner action from tool input
                tc_input = tool_call["input"]
                tc_name = tool_call["name"]
                # rye_execute -> execute, rye_search -> search, etc.
                inner_primary = tc_name.replace("rye_", "", 1) if tc_name.startswith("rye_") else tc_name
                inner_item_type = tc_input.get("item_type", "tool")
                # search has no item_id (uses query), load/sign/execute do
                inner_item_id = tc_input.get("item_id", "")

                denied = harness.check_permission(inner_primary, inner_item_type, inner_item_id)
                if denied:
                    clean = denied
                    emitter.emit(
                        thread_id,
                        "tool_call_result",
                        {"call_id": tool_call["id"], "output": str(clean), "error": denied["error"]},
                        transcript,
                    )
                    messages.append({
                        "role": "tool",
                        "tool_call_id": tool_call["id"],
                        "content": str(clean),
                    })
                    continue

                # directive_return: completion signal with structured outputs.
                # Detected by inner item_id before dispatch. Outputs are
                # extracted from the call parameters (not the tool result)
                # to avoid envelope/unwrapping fragility.
                if inner_item_id == "rye/agent/threads/directive_return":
                    inner_params = tc_input.get("parameters", {})
                    outputs = inner_params if isinstance(inner_params, dict) else {}

                    # Validate required fields against harness
                    missing = [
                        f for f in getattr(harness, "output_fields", [])
                        if not outputs.get(f)
                    ]
                    if missing:
                        error_msg = (
                            f"Missing required output fields: {', '.join(missing)}. "
                            f"Call directive_return again with all required fields."
                        )
                        emitter.emit(
                            thread_id,
                            "tool_call_result",
                            {"call_id": tool_call["id"], "output": error_msg, "error": error_msg},
                            transcript,
                        )
                        messages.append({
                            "role": "tool",
                            "tool_call_id": tool_call["id"],
                            "content": error_msg,
                        })
                        continue

                    emitter.emit(
                        thread_id,
                        "tool_call_result",
                        {"call_id": tool_call["id"], "output": str(outputs)},
                        transcript,
                    )

                    # Fire directive_return hook event
                    await harness.run_hooks(
                        "directive_return",
                        {"outputs": outputs, "cost": cost, "thread_id": thread_id},
                        dispatcher,
                        thread_ctx,
                    )

                    return _finalize(
                        thread_id,
                        cost,
                        {
                            "success": True,
                            "result": response["text"],
                            "outputs": outputs,
                        },
                        emitter,
                        transcript,
                        signer,
                    )

                resolved_id = tool_id_map.get(tool_call["name"], tool_call["name"])
                dispatch_params = dict(tool_call["input"])

                # Auto-inject parent context for child thread spawns
                if resolved_id == "rye/agent/threads/thread_directive":
                    dispatch_params.setdefault("parent_thread_id", thread_id)
                    dispatch_params.setdefault("parent_depth", orchestrator.get_depth(thread_id))
                    dispatch_params.setdefault("parent_limits", harness.limits)
                    dispatch_params.setdefault("parent_capabilities", harness._capabilities)

                result = await dispatcher.dispatch(
                    {
                        "primary": "execute",
                        "item_type": "tool",
                        "item_id": resolved_id,
                        "params": dispatch_params,
                    },
                    thread_context=thread_ctx,
                )

                clean = _clean_tool_result(result)

                # Guard: bound large results, dedupe, store artifacts
                context_ratio = _estimate_context_ratio(messages, provider)
                guarded = tool_result_guard.guard_result(
                    clean,
                    call_id=tool_call["id"],
                    tool_name=tool_call["name"],
                    thread_id=thread_id,
                    project_path=project_path,
                    context_usage_ratio=context_ratio,
                )

                emitter.emit(
                    thread_id,
                    "tool_call_result",
                    {"call_id": tool_call["id"], "output": str(guarded)},
                    transcript,
                )

                messages.append(
                    {
                        "role": "tool",
                        "tool_call_id": tool_call["id"],
                        "content": str(guarded),
                    }
                )

            # Post-turn hooks
            await harness.run_hooks(
                "after_step", {"cost": cost, "thread_id": thread_id}, dispatcher, thread_ctx
            )

            # Update cost snapshot in registry (post-turn)
            try:
                registry = thread_registry.get_registry(project_path)
                registry.update_cost_snapshot(thread_id, cost)
            except Exception:
                pass  # cost snapshot is best-effort

            # Context limit check — handoff to a new thread
            limit_info = _check_context_limit(messages, provider, project_path)
            if limit_info:
                emitter.emit(
                    thread_id,
                    "context_limit_reached",
                    limit_info,
                    transcript,
                    criticality="critical",
                )
                try:
                    handoff_result = await orchestrator.handoff_thread(
                        thread_id, project_path, messages=list(messages),
                    )
                    if handoff_result.get("success"):
                        return _finalize(
                            thread_id,
                            cost,
                            {
                                "success": True,
                                "status": "continued",
                                "continuation_thread_id": handoff_result.get("new_thread_id"),
                                "handoff": handoff_result,
                            },
                            emitter,
                            transcript,
                            signer,
                        )
                except Exception as handoff_err:
                    logger.error("Handoff failed: %s", handoff_err)
                # Handoff failed — fall back to hook-based handling
                hook_result = await harness.run_hooks(
                    "context_limit_reached", limit_info, dispatcher, thread_ctx
                )
                if hook_result and hook_result.get("action") == "continue":
                    return _finalize(
                        thread_id,
                        cost,
                        {
                            "success": True,
                            "status": "continued",
                            "continuation_thread_id": hook_result.get("continuation_thread_id"),
                        },
                        emitter,
                        transcript,
                        signer,
                    )

    finally:
        cost["elapsed_seconds"] = time.monotonic() - start_time
        final = {
            **cost,
            "status": cost.get("_status", "completed" if cost.get("turns") else "error"),
        }
        orchestrator.complete_thread(thread_id, final)

        transcript.render_knowledge_transcript(
            directive=harness.directive_name,
            status=final["status"],
            model=provider.model,
            cost=cost,
        )

        # Dispatch after_complete hooks (best-effort)
        try:
            await harness.run_hooks(
                "after_complete",
                {"thread_id": thread_id, "cost": cost, "project_path": str(project_path)},
                dispatcher,
                {"emitter": emitter, "transcript": transcript, "thread_id": thread_id},
            )
        except Exception:
            pass  # after_complete hooks must not break thread finalization


def _finalize(thread_id, cost, result, emitter, transcript, signer=None) -> Dict:
    if signer and cost.get("turns"):
        signer.checkpoint(cost["turns"])
    # Preserve explicit status (e.g. "continued", "cancelled") over default
    if "status" in result and result["status"] not in ("", None):
        status = result["status"]
    elif result.get("success"):
        status = "completed"
    else:
        status = "error"
    if not result.get("success") and not result.get("error"):
        result["error"] = "Unknown error (no message provided)"
    emit_payload = {"cost": cost}
    if result.get("error"):
        emit_payload["error"] = result["error"]
    emitter.emit(
        thread_id, f"thread_{status}", emit_payload, transcript, criticality="critical"
    )
    # Record status in cost so the finally block uses the authoritative value
    cost["_status"] = status
    return {**result, "thread_id": thread_id, "cost": cost, "status": status}


def _clean_tool_result(result: Any) -> Any:
    """Strip chain/metadata bloat from rye execute results.

    Unwraps the rye_execute envelope to get the inner tool result.
    Drops chain, metadata, resolved_env_keys, path, source.
    Strips rye signature headers from content fields.
    """
    if not isinstance(result, dict):
        return result

    DROP_KEYS = frozenset(("chain", "metadata", "path", "source", "resolved_env_keys"))

    def _strip(d: dict) -> dict:
        cleaned = {k: v for k, v in d.items() if k not in DROP_KEYS}
        # Strip rye signature line from content
        if "content" in cleaned and isinstance(cleaned["content"], str):
            cleaned["content"] = _strip_signature(cleaned["content"])
        return cleaned

    # Unwrap rye_execute envelope: {status, type, item_id: "rye/primary-tools/rye_execute", data: {actual result}}
    inner = result.get("data")
    if isinstance(inner, dict) and result.get("item_id", "").startswith("rye/primary/"):
        return _strip(inner)

    return _strip(result)


def _strip_signature(text: str) -> str:
    """Remove rye signature lines from content."""
    lines = text.split("\n")
    cleaned = [l for l in lines if not l.strip().startswith(("# rye:signed:", "<!-- rye:signed:"))]
    return "\n".join(cleaned).strip()


def _check_context_limit(messages, provider, project_path):
    """Check if context window is approaching capacity.

    Returns event dict if threshold crossed, else None.
    """
    tokens_used = _estimate_message_tokens(messages)
    context_limit = getattr(provider, "context_window", None)
    if not context_limit:
        context_limit = provider.config.get("context_window", 200000) if hasattr(provider, "config") else 200000
    if context_limit <= 0:
        return None

    usage_ratio = tokens_used / context_limit

    # Default threshold 0.9 — load from coordination config if available
    threshold = 0.9
    try:
        coordination_loader = load_module("loaders/coordination_loader", anchor=_ANCHOR)
        config = coordination_loader.load(project_path)
        threshold = config.get("coordination", {}).get("continuation", {}).get("trigger_threshold", 0.9)
    except Exception:
        pass

    if usage_ratio >= threshold:
        return {
            "usage_ratio": usage_ratio,
            "tokens_used": tokens_used,
            "tokens_limit": context_limit,
        }

    return None


def _estimate_message_tokens(messages):
    """Rough token estimate: ~4 chars per token for English text."""
    total_chars = sum(len(str(m.get("content", ""))) for m in messages)
    return total_chars // 4


def _estimate_context_ratio(messages, provider):
    """Current context usage as a ratio (0.0 to 1.0)."""
    tokens_used = _estimate_message_tokens(messages)
    context_limit = getattr(provider, "context_window", None)
    if not context_limit:
        context_limit = provider.config.get("context_window", 200000) if hasattr(provider, "config") else 200000
    return tokens_used / context_limit if context_limit > 0 else 0.0


def _extract_error_context(messages: List[Dict], max_entries: int = 6) -> Dict:
    """Extract recent conversation context for error diagnostics.

    Captures the last few messages so the parent/orchestrator can
    understand what the model was doing when it hit a limit.
    Returns: {last_assistant, recent_tool_calls, recent_errors}
    """
    last_assistant = ""
    recent_tool_calls = []
    recent_errors = []

    for msg in reversed(messages[-max_entries * 2:]):
        role = msg.get("role", "")
        content = str(msg.get("content", ""))

        if role == "assistant" and not last_assistant:
            last_assistant = content[:500] if content else ""

        elif role == "tool":
            snippet = content[:300]
            if "error" in content.lower() or "denied" in content.lower():
                recent_errors.append(snippet)
            else:
                recent_tool_calls.append(snippet)

            if len(recent_tool_calls) + len(recent_errors) >= max_entries:
                break

    return {
        "last_assistant": last_assistant,
        "recent_tool_calls": recent_tool_calls[:3],
        "recent_errors": recent_errors[:3],
    }


def _emit_context_injected(hook_ctx, emitter, thread_id, transcript):
    """Emit context_injected events for transcript observability."""
    for position in ("before", "after"):
        raw_key = f"{position}_raw"
        blocks = hook_ctx.get(raw_key, [])
        if blocks:
            emitter.emit(
                thread_id,
                "context_injected",
                {"position": position, "blocks": blocks},
                transcript,
            )


def _error_to_context(e: Exception) -> Dict:
    """Convert exception to context dict for error classification."""
    ctx = {
        "error": {
            "type": type(e).__name__,
            "message": str(e),
            "code": getattr(e, "code", None),
        }
    }
    # Surface http_status for classifier pattern matching
    if hasattr(e, "http_status") and e.http_status is not None:
        ctx["status_code"] = e.http_status
    if os.environ.get("RYE_DEBUG"):
        import traceback
        ctx["error"]["class_hierarchy"] = [c.__name__ for c in type(e).__mro__]
        ctx["error"]["traceback"] = traceback.format_exc()
    return ctx
