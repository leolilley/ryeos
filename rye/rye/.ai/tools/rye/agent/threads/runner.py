# rye:signed:2026-02-16T05:55:29Z:25a3c3361b342464564be606445bdac19822953a3f9ebea6e89436710c9c857c:VaCHtfmENAKOgJCwf2IAzGOrGU6jeKXDDuP-pEsfrIlmW179A4h33LdnEFwRhngpw96DHiPRllS4heEmYg4PDQ==:440443d0858f0199
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

__version__ = "1.1.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads"
__tool_description__ = "Core LLM loop for thread execution"

import asyncio
import logging
import os
import time
from pathlib import Path
from typing import Any, Dict

from module_loader import load_module

logger = logging.getLogger(__name__)

_ANCHOR = Path(__file__).parent

orchestrator = load_module("orchestrator", anchor=_ANCHOR)


async def run(
    thread_id: str,
    user_prompt: str,
    harness: "SafetyHarness",
    provider: "ProviderAdapter",
    dispatcher: "ToolDispatcher",
    emitter: "EventEmitter",
    transcript: Any,
    project_path: Path,
) -> Dict:
    """Execute the LLM loop until completion, error, or limit.

    No system prompt. Tools are passed via API tool definitions.
    Context framing (identity, rules, etc.) injected via thread_started hooks.

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

    try:
        # --- Build first message ---
        # Hook context: thread_started hooks load knowledge items (identity, rules)
        hook_context = await harness.run_hooks_context(
            {
                "directive": harness.directive_name,
                "model": provider.model,
                "limits": harness.limits,
            },
            dispatcher,
        )

        first_message_parts = []
        if hook_context:
            first_message_parts.append(hook_context)
        first_message_parts.append(user_prompt)
        messages.append({"role": "user", "content": "\n\n".join(first_message_parts)})

        while True:
            # Pre-turn limit check
            cost["elapsed_seconds"] = time.monotonic() - start_time
            limit_result = harness.check_limits(cost)
            if limit_result:
                hook_result = await harness.run_hooks(
                    "limit", limit_result, dispatcher, thread_ctx
                )
                if hook_result:
                    return _finalize(
                        thread_id, cost, hook_result, emitter, transcript
                    )

            # Cancellation check
            if harness.is_cancelled():
                return _finalize(
                    thread_id,
                    cost,
                    {"success": False, "status": "cancelled"},
                    emitter,
                    transcript,
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
                response = await provider.create_completion(
                    messages, harness.available_tools
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
                        thread_id, cost, hook_result, emitter, transcript
                    )
                return _finalize(
                    thread_id, cost, original_error, emitter, transcript
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

            # Process tool calls
            tool_calls = response.get("tool_calls", [])
            if not tool_calls:
                return _finalize(
                    thread_id,
                    cost,
                    {"success": True, "result": response["text"]},
                    emitter,
                    transcript,
                )

            # Append assistant message (with tool_use blocks) to conversation
            messages.append({"role": "assistant", "content": response["text"], "tool_calls": tool_calls})

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

                emitter.emit(
                    thread_id,
                    "tool_call_result",
                    {"call_id": tool_call["id"], "output": str(clean)},
                    transcript,
                )

                messages.append(
                    {
                        "role": "tool",
                        "tool_call_id": tool_call["id"],
                        "content": str(clean),
                    }
                )

            # Post-turn hooks
            await harness.run_hooks(
                "after_step", {"cost": cost, "thread_id": thread_id}, dispatcher, thread_ctx
            )

    finally:
        final = {
            **cost,
            "status": "completed" if cost.get("turns") else "error",
        }
        orchestrator.complete_thread(thread_id, final)


def _finalize(thread_id, cost, result, emitter, transcript) -> Dict:
    status = "completed" if result.get("success") else result.get("status", "error")
    if not result.get("success") and not result.get("error"):
        result["error"] = "Unknown error (no message provided)"
    emitter.emit(
        thread_id, f"thread_{status}", {"cost": cost}, transcript, criticality="critical"
    )
    return {**result, "thread_id": thread_id, "cost": cost, "status": status}


def _clean_tool_result(result: Any) -> Any:
    """Strip chain/metadata bloat from rye execute results.

    Unwraps the rye_execute envelope to get the inner tool result.
    Drops chain, metadata, resolved_env_keys.
    """
    if not isinstance(result, dict):
        return result

    def _strip(d: dict) -> dict:
        return {k: v for k, v in d.items() if k not in ("chain", "metadata")}

    # Unwrap rye_execute envelope: {status, type, item_id: "rye/primary-tools/rye_execute", data: {actual result}}
    inner = result.get("data")
    if isinstance(inner, dict) and result.get("item_id", "").startswith("rye/primary-tools/"):
        return _strip(inner)

    return _strip(result)


def _error_to_context(e: Exception) -> Dict:
    """Convert exception to context dict for error classification."""
    ctx = {
        "error": {
            "type": type(e).__name__,
            "message": str(e),
            "code": getattr(e, "code", None),
        }
    }
    if os.environ.get("RYE_DEBUG"):
        import traceback
        ctx["error"]["class_hierarchy"] = [c.__name__ for c in type(e).__mro__]
        ctx["error"]["traceback"] = traceback.format_exc()
    return ctx
