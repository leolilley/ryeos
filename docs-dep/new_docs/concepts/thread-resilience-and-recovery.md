# Thread Resilience and Recovery

How threads handle interruptions, recover from failures, and let users adjust limits to resume stopped work. Covers crash recovery, transient error retry, limit escalation, orphan detection, and hierarchical failure propagation.

**Configuration:** All resilience behavior is data-driven from YAML. See [data-driven-resilience-config.md](data-driven-resilience-config.md) for retry policies and budget limits. See [data-driven-state-persistence.md](data-driven-state-persistence.md) for checkpoint triggers and state.json schema.

This document extends [thread-orchestration-internals.md](thread-orchestration-internals.md) with the resilience layer that makes orchestration production-grade.

## The Problem Space

A thread can stop for many reasons:

| Category                | Examples                                                                  | Current Handling                                                                |
| ----------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| **API transient**       | 429 rate limit, 503 service unavailable, connection reset, socket timeout | LLM call fails → hook evaluates → thread errors out                             |
| **Cost limit**          | `spend >= max_spend`, `turns >= max_turns`, `tokens >= max_tokens`        | `check_limits()` returns event → hook evaluates → but loop doesn't act on RETRY |
| **Process crash**       | Python exception, OOM kill, SIGKILL, power loss                           | Thread stays "running" in registry forever — orphaned                           |
| **Child failure**       | Child thread errors → parent discovers via completion event               | Push-based via `asyncio.Event` + `wait_threads` fail_fast                       |
| **Hierarchical budget** | Parent's $3.00 budget exhausted mid-wave                                  | Children keep running (per gap 5 in orchestration doc)                          |
| **User cancellation**   | User wants to stop a runaway thread                                       | No cancellation API                                                             |

### What Exists Today

The system has building blocks but doesn't connect them into a coherent recovery flow:

- **`HarnessAction.RETRY`** — defined in the enum but nothing in `_run_tool_use_loop` acts on it. The hook can return "retry" but the loop ignores the action.
- **`checkpoint_on_error()`** — evaluates hooks on error, but only for LLM failures. Will be removed entirely — replaced by the unified `classify_error()` → `evaluate_hooks()` → default behavior cascade described in this document.
- **`save_state()`** — exists in `core_helpers.py` but `thread_directive.execute()` never calls it. Only `conversation_mode.py` persists harness state. A crashed single-turn thread has no `state.json` to resume from.
- **`from_state_dict()`** — can restore a harness, but only if state was saved before the crash.
- **Thread status** — registry tracks `running | completed | error` today, but there's no `suspended` or `cancelled` state (both are planned additions).
- **Approval flow** — file-based approval request/response exists but isn't wired into limit escalation.

### What's Missing

1. **State persistence at every checkpoint** — save harness state before and after each LLM turn, not just in conversation mode
2. **Error classification** — distinguish transient (retry) from permanent (fail) errors
3. **Retry loop with backoff** — act on `HarnessAction.RETRY` in the tool-use loop
4. **Limit escalation** — suspend thread, request approval to bump limits, resume
5. **Crash recovery** — detect orphaned "running" threads, reconstruct state, offer resume
6. **Cancellation** — stop a running thread gracefully from outside
7. **Failure propagation** — push-based child→parent notification via completion events, not polling

---

## 1. Checkpoint-Based State Persistence

### Problem

`thread_directive.execute()` runs the entire LLM loop without saving state. If the process crashes on turn 7 of 10, all progress is lost — transcript events exist in JSONL but there's no `state.json` to restore the harness from.

### Design

Save harness state at every checkpoint in the tool-use loop. The cost is minimal — one atomic JSON write per LLM turn.

```
Turn 1: [save state] → call LLM → [save state] → execute tools → [save state]
Turn 2: [save state] → call LLM → [save state] → execute tools → [save state]
...crash...
Resume: [load state] → rebuild messages from transcript → continue from turn N
```

### Changes to `thread_directive.py`

Add state saves to `_run_tool_use_loop()`:

> **Import convention:** Code snippets in this doc use bare module names (`from core_helpers import ...`) because all thread tools live in the same directory (`rye/rye/.ai/tools/rye/agent/threads/`) and are loaded via `importlib` at runtime. These are not Python package imports.

```python
from core_helpers import save_state

async def _run_tool_use_loop(
    ...
    thread_id: str = "",
    ...
) -> Dict[str, Any]:
    ...
    for turn in range(MAX_TOOL_ROUNDTRIPS):
        # Checkpoint: save state before LLM call
        if thread_id:
            try:
                save_state(thread_id, harness, project_path)
            except Exception as e:
                logger.warning(f"Failed to save pre-turn state: {e}")

        llm_result = await _call_llm_with_retry(...)

        # Checkpoint: save state after LLM response (cost updated)
        harness.update_cost_after_turn(...)
        if thread_id:
            try:
                save_state(thread_id, harness, project_path)
            except Exception as e:
                logger.warning(f"Failed to save post-turn state: {e}")

        ...tool execution...
```

Note: this adds up to three writes per turn (pre-LLM, post-LLM, post-tools).
If I/O becomes a bottleneck, consider batching to a single state write per
turn (post-tools) and keep a lightweight in-memory checkpoint for crash safety.

### Changes to `execute()`

Save state immediately after thread registration, before the LLM loop starts:

```python
# After registry.register() and before _run_tool_use_loop()
try:
    save_state(thread_id, harness, project_path)
except Exception as e:
    logger.warning(f"Failed to save initial state: {e}")
```

### State File Contents

`state.json` already contains everything needed to resume:

```json
{
  "directive": "implement_feature",
  "inputs": {"plan": "plan_db_schema"},
  "cost": {
    "turns": 4,
    "tokens": 12500,
    "input_tokens": 8200,
    "output_tokens": 4300,
    "spawns": 0,
    "spend": 0.23,
    "duration_seconds": 45.2
  },
  "limits": {
    "turns": 15,
    "tokens": 50000,
    "spend": 1.00,
    "duration": 300
  },
  "hooks": [...],
  "required_caps": ["rye.execute.tool.apps_task-manager_*"]
}
```

Combined with `transcript.jsonl` (which already captures every message, tool call, and tool result), this is sufficient to fully reconstruct the thread's execution state.

---

## Architecture: Hooks as the Policy Layer

The resilience mechanisms below are layered on top of the existing hook system — they don't bypass it. The hook system remains the user-extensible policy layer; the mechanisms in this doc provide **default behaviors** that activate when no user hook matches.

### Three Layers

```
┌──────────────────────────────────────────────────┐
│  Directive XML (user-defined hooks)              │
│  <hook event="limit" when="spend_exceeded">      │
│    <directive>my_custom_escalation</directive>    │
│  </hook>                                         │
├──────────────────────────────────────────────────┤
│  Default behaviors (this document)               │
│  _call_llm_with_retry, _handle_limit_hit,        │
│  error_classifier — activate when no hook matches│
├──────────────────────────────────────────────────┤
│  Infrastructure (not hookable)                   │
│  signal_completion(), asyncio.Event, registry,   │
│  checkpoint saves, transcript writes             │
└──────────────────────────────────────────────────┘
```

**User hooks take priority.** When `check_limits()` fires a `limit` event, `evaluate_hooks()` checks user-defined hooks first. If a hook's `when` expression matches, its directive runs and its returned `HarnessAction` controls the loop. Only if **no hook matches** does the default behavior (`_handle_limit_hit`) activate. The `when` evaluator should remain intentionally small (no function calls), but we may want to add simple helpers like `starts_with()` or numeric comparisons against nested fields to support expressions such as `cost.spend > 2.0` without custom code.

**Error retry follows the same pattern.** `_call_llm_with_retry` is the default retry policy. But the tool-use loop evaluates hooks at each error checkpoint. A user can define:

```xml
<hook event="error" when="error_category == 'rate_limited'">
  <directive>my_custom_backoff</directive>
</hook>
```

If this hook matches, the custom directive runs and returns `RETRY`, `FAIL`, or `ABORT`. The default `_call_llm_with_retry` only handles errors that reach it without a hook intercepting first.

**Infrastructure is below hooks.** Completion events (`signal_completion`), checkpoint saves, registry updates, and transcript writes are runtime primitives. They always execute regardless of hook configuration. Users don't hook into "thread completed" — they hook into semantic events like `after_complete`, `limit`, or `error` that fire before the infrastructure layer acts.

### The Decision Cascade

At each checkpoint in the tool-use loop:

```
1. check_limits() or classify_error()  → produces an event
2. evaluate_hooks(event)                → checks user-defined hooks
   ├── Hook matches → run hook directive → HarnessAction returned
   │   ├── RETRY    → loop continues
   │   ├── FAIL     → thread errors, signal_completion fires
   │   ├── ABORT    → thread errors, signal_completion fires
   │   └── CONTINUE → proceed to default behavior
   └── No hook matches → HarnessAction.CONTINUE (default)
3. Default behavior activates (if no hook overrode)
   ├── Limit event  → _handle_limit_hit (suspend + escalation)
   ├── Transient error → _call_llm_with_retry (exponential backoff)
   └── Permanent error → thread errors
4. Infrastructure (always runs)
   └── signal_completion(), save_state(), transcript write
```

This means:

- A user who wants custom retry backoff writes a hook for `error` events
- A user who wants custom limit handling writes a hook for `limit` events
- A user who wants context window compaction writes a hook for `context_window_pressure` events
- A user who wants to be notified on completion writes a hook for `after_complete`
- All of these compose — multiple hooks can match different `when` conditions
- The defaults in this document are the fallback when no user hook applies

The `context_window_pressure` event demonstrates this pattern clearly: the harness has **no built-in compaction logic**. It emits the event with a pre-computed `pressure_ratio`; a user-defined hook directive does the actual summarization; the tool-use loop applies the result via a generic "apply context patch" mechanism. Policy chooses _when_ and _how_; infrastructure provides a generic way to apply the result. See [Compaction and Pruning](thread-orchestration-internals.md#compaction-and-pruning) for the full design.

---

## 2. Error Classification and Retry

> This section describes the **default** retry behavior. User-defined `<hook event="error">` directives take priority — see [Hooks as the Policy Layer](#architecture-hooks-as-the-policy-layer) above.

### Problem

All errors are treated the same. A 429 rate limit (wait 30 seconds and retry) gets the same treatment as a malformed directive (never going to work). The `HarnessAction.RETRY` action exists but the tool-use loop doesn't act on it.

### Error Taxonomy

```python
class ErrorCategory(Enum):
    """Classification of execution errors."""

    TRANSIENT = "transient"       # Retry after backoff (429, 503, timeout, connection reset)
    RATE_LIMITED = "rate_limited"  # Retry after specific delay (429 with Retry-After header)
    QUOTA = "quota"               # API quota exhausted — retry after longer delay or escalate
    LIMIT_HIT = "limit_hit"       # Thread's own limit reached — escalate to user
    BUDGET = "budget"             # Hierarchical budget exhausted — escalate to parent
    PERMANENT = "permanent"       # Cannot recover (bad directive, missing tool, auth failure)
    CANCELLED = "cancelled"       # User-initiated cancellation

ERROR_PATTERNS = {
    # HTTP status codes from LLM providers
    429: ErrorCategory.RATE_LIMITED,
    503: ErrorCategory.TRANSIENT,
    502: ErrorCategory.TRANSIENT,
    500: ErrorCategory.TRANSIENT,
    408: ErrorCategory.TRANSIENT,

    # Error message patterns (regex → category)
    r"rate.?limit": ErrorCategory.RATE_LIMITED,
    r"(connection|connect).*(reset|refused|timeout)": ErrorCategory.TRANSIENT,
    r"(socket|read).?timeout": ErrorCategory.TRANSIENT,
    r"overloaded": ErrorCategory.TRANSIENT,
    r"quota.*(exceeded|exhausted)": ErrorCategory.QUOTA,
    r"(invalid|malformed).*(api.?key|token|auth)": ErrorCategory.PERMANENT,
    r"model.?not.?found": ErrorCategory.PERMANENT,
    r"content.?policy": ErrorCategory.PERMANENT,
}

ERROR_PATTERNS should be loaded from a YAML config (project, user, system) so
operators can extend categories without editing code. The default patterns live
in the system tool bundle, and project overrides merge/extend them.
```

### Classifier

```python
def classify_error(error: Union[str, Dict, Exception]) -> Tuple[ErrorCategory, Dict]:
    """Classify an error and extract metadata.

    Returns (category, metadata) where metadata may include:
    - retry_after: seconds to wait (from Retry-After header or default)
    - status_code: HTTP status if available
    - message: error message string
    """
    metadata = {}

    # Extract error details
    if isinstance(error, dict):
        status = error.get("status_code") or error.get("status")
        message = error.get("error") or error.get("message") or str(error)
        metadata["retry_after"] = error.get("retry_after")
    elif isinstance(error, Exception):
        message = str(error)
        status = getattr(error, "status_code", None)
    else:
        message = str(error)
        status = None

    metadata["message"] = message
    if status:
        metadata["status_code"] = status

    # Check status code first
    if status and status in ERROR_PATTERNS:
        category = ERROR_PATTERNS[status]
        if category == ErrorCategory.RATE_LIMITED and not metadata.get("retry_after"):
            metadata["retry_after"] = 30  # Default 429 backoff
        return category, metadata

    # Check message patterns
    import re
    for pattern, category in ERROR_PATTERNS.items():
        if isinstance(pattern, str) and re.search(pattern, message, re.IGNORECASE):
            return category, metadata

    # Default: permanent (fail safe — don't retry unknown errors)
    return ErrorCategory.PERMANENT, metadata
```

### Retry Loop

The retry logic wraps `_call_llm` inside `_run_tool_use_loop`:

```python
config.retry.max_retries (default: 3, from resilience.yaml)
config.retry.policies.exponential.base (default: 2.0, from resilience.yaml)  # seconds
config.retry.policies.exponential.max_delay (default: 120.0, from resilience.yaml)  # seconds

async def _call_llm_with_retry(
    project_path: Path,
    model: str,
    system_prompt: str,
    messages: List[Dict],
    max_tokens: int,
    provider_id: str,
    tools: Optional[List[Dict]],
    harness: SafetyHarness,
    thread_id: str = "",
    transcript: Optional[Any] = None,
    directive_name: str = "",
) -> Dict[str, Any]:
    """Call LLM with automatic retry for transient errors.

    Retry policy:
    - TRANSIENT: retry up to MAX_RETRIES with exponential backoff
    - RATE_LIMITED: retry after Retry-After delay (or default 30s)
    - QUOTA: retry once after 60s, then escalate
    - PERMANENT: fail immediately
    - LIMIT_HIT / BUDGET: don't retry, escalate
    """
    last_error = None

    for attempt in range(MAX_RETRIES + 1):
        result = await _call_llm(
            project_path=project_path,
            model=model,
            system_prompt=system_prompt,
            messages=messages,
            max_tokens=max_tokens,
            provider_id=provider_id,
            tools=tools,
        )

        if result["success"]:
            if attempt > 0 and transcript and thread_id:
                transcript.write_event(thread_id, "retry_succeeded", {
                    "directive": directive_name,
                    "attempt": attempt + 1,
                    "previous_error": str(last_error),
                })
            return result

        # Classify the error
        category, meta = classify_error(result.get("error", ""))
        last_error = meta.get("message", result.get("error"))

        # Emit retry event
        if transcript and thread_id:
            transcript.write_event(thread_id, "error_classified", {
                "directive": directive_name,
                "category": category.value,
                "attempt": attempt + 1,
                "error": last_error,
                "retry_after": meta.get("retry_after"),
            })

        # Decision based on category
        if category == ErrorCategory.PERMANENT:
            return result  # No retry

        if category in (ErrorCategory.LIMIT_HIT, ErrorCategory.BUDGET, ErrorCategory.CANCELLED):
            return result  # Escalate, don't retry

        if attempt >= MAX_RETRIES:
            return result  # Exhausted retries

        # Calculate backoff
        if category == ErrorCategory.RATE_LIMITED:
            delay = meta.get("retry_after", 30)
        elif category == ErrorCategory.QUOTA:
            delay = 60
        else:
            delay = calculate_delay(config.retry.policies.exponential, attempt)

        logger.info(f"Retry {attempt + 1}/{MAX_RETRIES} after {delay}s ({category.value})")

        # Save state before sleeping (crash protection)
        if thread_id:
            try:
                save_state(thread_id, harness, project_path)
            except Exception:
                pass

        await asyncio.sleep(delay)

    return result

Retry policy values (MAX_RETRIES, BACKOFF_BASE/MAX, QUOTA delay) should be
loaded from directive metadata (e.g., `<limits retry_max="..." backoff_base="..." />`)
or provider config defaults to keep behavior data-driven. Provider configs may also
include retry-specific settings in their `retry` section (e.g., `retry.max_attempts`,
`retry.backoff`).
```

> **opencode reference:** opencode's `session/retry.ts` implements the same exponential backoff pattern with two refinements worth adopting: (1) **Dual header parsing** — handles both `retry-after-ms` (milliseconds) and `retry-after` (seconds or HTTP date format); our `classify_error()` should extract both. (2) **Abort-aware sleep** — the retry delay uses `signal.addEventListener("abort", ...)` so cancellation interrupts the wait immediately; our `asyncio.sleep()` in `_call_llm_with_retry` should use `asyncio.wait_for` with a cancellation event. (3) **Provider-specific retryability** — opencode marks OpenAI 404s as retryable (model availability flapping) and parses stream errors for `insufficient_quota` and `context_length_exceeded` distinctly.

### Acting on RETRY from Hooks

When `check_limits()` fires and a hook returns `RETRY`, the tool-use loop now respects it:

```python
# In _run_tool_use_loop, after harness.update_cost_after_turn():
limit_event = harness.check_limits()
if limit_event:
    # Step 1: Check user-defined hooks (policy layer)
    hook_result = harness.evaluate_hooks(limit_event)

    if hook_result.action == HarnessAction.RETRY:
        continue  # Hook says retry (e.g., custom escalation approved)

    if hook_result.action == HarnessAction.ABORT:
        return {"success": False, "error": hook_result.error or "Aborted by hook", "abort": True}

    if hook_result.action == HarnessAction.FAIL:
        return {"success": False, "error": hook_result.error or "Failed by hook"}

    # Step 2: No hook matched (CONTINUE) → fall through to default behavior
    # Default: suspend thread and request limit escalation
    suspension = await _handle_limit_hit(
        harness=harness,
        limit_event=limit_event,
        thread_id=thread_id,
        project_path=project_path,
        transcript=transcript,
        registry=registry,
    )
    return suspension  # Thread suspended, awaiting external resume

The error path must mirror this structure. After classifying a failed LLM/tool
call, evaluate hooks for the `error` event; if the hook returns RETRY, re-enter
_call_llm_with_retry. If CONTINUE and the error is transient, use the default
retry. If permanent, mark error. This avoids the current fall-through gap where
error hooks can return CONTINUE but no retry path exists.
```

---

## 3. Limit Escalation

> This section describes the **default** escalation behavior. User-defined `<hook event="limit">` directives take priority — see [Hooks as the Policy Layer](#architecture-hooks-as-the-policy-layer) above.

### Problem

When a thread hits `max_spend` or `max_turns`, it just stops. The user has no way to say "I approve spending $2 more" and resume where they left off. The work is lost unless the thread was in conversation mode with state saved.

### Design

Limit escalation uses the existing approval flow (Phase 3) as the human-in-the-loop mechanism. When a limit is hit:

1. Thread suspends (not errors)
2. Approval request is created with escalation details
3. Thread waits for human response
4. If approved, limits are bumped and thread resumes
5. If denied, thread completes with partial results

### Thread Status Extension

Add new statuses to represent escalation states:

```
running → suspended → running → completed
running → cancelled
```

The **registry (SQLite)** is the canonical source for thread status. `thread.json` may cache status for convenience, but the registry is authoritative. Always update both; always read from the registry first. The _reason_ for suspension lives in `state.json` as `suspend_reason`:

```python
# Thread status values (registry level — simple strings only)
THREAD_STATUSES = {
    "running",          # Currently executing
    "completed",        # Finished successfully
    "error",            # Permanent failure
    "suspended",        # Awaiting external action (reason in state.json suspend_reason)
    "cancelled",        # User-initiated cancellation
}

# suspend_reason values (stored in state.json, not in the registry)
SUSPEND_REASONS = {
    "limit",            # Thread's own limit hit, awaiting escalation approval
    "error",            # Transient error exhausted retries, awaiting user decision
    "budget",           # Hierarchical budget exhausted, awaiting parent/user
    "approval",         # Waiting for explicit approval
}
```

### Escalation Flow

```python
async def _handle_limit_hit(
    harness: SafetyHarness,
    limit_event: Dict,
    thread_id: str,
    project_path: Path,
    transcript: Optional[Any] = None,
    registry: Optional[Any] = None,
) -> Dict[str, Any]:
    """Handle a limit being hit by requesting escalation.

    Flow:
    1. Save current state (so we can resume)
    2. Create escalation request with current usage and proposed new limits
    3. Update thread status to suspended (with suspend_reason in state.json)
    4. Return suspension result (caller decides whether to block or return)
    """
    # Save state for resume (includes suspend_reason)
    save_state(thread_id, harness, project_path)

    # Build escalation details
    limit_code = limit_event.get("code", "unknown")
    current_value = limit_event.get("current", 0)
    max_value = limit_event.get("max", 0)

    # Propose new limits (double the current limit)
    proposed_limits = dict(harness.limits)
    limit_key = _limit_code_to_key(limit_code)
    if limit_key and limit_key in proposed_limits:
        proposed_limits[limit_key] = proposed_limits[limit_key] * 2

    escalation = {
        "type": "limit_escalation",
        "thread_id": thread_id,
        "directive": harness.directive_name,
        "limit_code": limit_code,
        "current_value": current_value,
        "current_max": max_value,
        "proposed_max": proposed_limits.get(limit_key),
        "current_cost": harness.cost.to_dict(),
        "message": _format_escalation_message(limit_code, current_value, max_value, harness),
    }

    # Create approval request
    from approval_flow import request_approval
    request_id = request_approval(
        thread_id=thread_id,
        prompt=escalation["message"],
        project_path=project_path,
    )
    escalation["approval_request_id"] = request_id

    # Write escalation to thread metadata
    escalation_path = project_path / ".ai" / "threads" / thread_id / "escalation.json"
    tmp_path = escalation_path.with_suffix(".json.tmp")
    try:
        with open(tmp_path, "w", encoding="utf-8") as f:
            json.dump(escalation, f, indent=2)
        tmp_path.rename(escalation_path)
    except Exception as e:
        logger.error(f"Failed to write escalation: {e}")

    # Update thread status (reason goes in state.json as suspend_reason, not in registry)
    if registry:
        registry.update_status(thread_id, "suspended")

    # Emit lifecycle event (critical, infrastructure)
    if transcript:
        transcript.write_event(thread_id, "thread_suspended", {
            "directive": harness.directive_name,
            "suspend_reason": "limit",
            "cost": harness.cost.to_dict(),
            "limit_code": limit_code,
        })

    # Emit escalation event (critical, default behavior)
    if transcript:
        transcript.write_event(thread_id, "limit_escalation_requested", escalation)

    return {
        "status": "suspended",
        "thread_id": thread_id,
        "escalation": escalation,
        "approval_request_id": request_id,
    }

The limit escalation policy ("double the limit") should be configurable from
directive metadata or a per-project policy file. Default to doubling, but allow
fixed increments, absolute ceilings, or denial-only behavior.


def _limit_code_to_key(code: str) -> Optional[str]:
    """Map limit event code to limits dict key."""
    return {
        "turns_exceeded": "turns",
        "tokens_exceeded": "tokens",
        "spend_exceeded": "spend",
        "spawns_exceeded": "spawns",
        "duration_exceeded": "duration",
        "hierarchical_budget_exceeded": "spend",
    }.get(code)


def _format_escalation_message(
    code: str, current: Any, max_val: Any, harness: SafetyHarness
) -> str:
    """Format human-readable escalation prompt."""
    cost = harness.cost.to_dict()
    directive = harness.directive_name

    if code == "spend_exceeded":
        return (
            f"Thread '{directive}' has reached its spend limit.\n\n"
            f"Current spend: ${cost['spend']:.4f}\n"
            f"Limit: ${max_val}\n"
            f"Turns used: {cost['turns']}\n"
            f"Tokens used: {cost['tokens']:,}\n\n"
            f"Approve to increase spend limit to ${max_val * 2}?"
        )
    elif code == "turns_exceeded":
        return (
            f"Thread '{directive}' has reached its turn limit.\n\n"
            f"Turns used: {cost['turns']}\n"
            f"Limit: {max_val}\n"
            f"Current spend: ${cost['spend']:.4f}\n\n"
            f"Approve to increase turn limit to {max_val * 2}?"
        )
    elif code == "tokens_exceeded":
        return (
            f"Thread '{directive}' has reached its token limit.\n\n"
            f"Tokens used: {cost['tokens']:,}\n"
            f"Limit: {max_val:,}\n"
            f"Current spend: ${cost['spend']:.4f}\n\n"
            f"Approve to increase token limit to {max_val * 2:,}?"
        )
    else:
        return (
            f"Thread '{directive}' has hit a limit: {code}\n\n"
            f"Current: {current}\n"
            f"Max: {max_val}\n\n"
            f"Approve to increase?"
        )
```

### Resume After Escalation

A new tool — `rye/agent/threads/resume_thread.py` — handles resumption:

```python
CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "thread_id": {
            "type": "string",
            "description": "Thread to resume",
        },
        "action": {
            "type": "string",
            "enum": ["resume", "bump_and_resume", "cancel"],
            "description": "What to do with the suspended thread",
        },
        "new_limits": {
            "type": "object",
            "description": "New limits to apply (for bump_and_resume)",
            "properties": {
                "spend": {"type": "number"},
                "turns": {"type": "integer"},
                "tokens": {"type": "integer"},
                "duration": {"type": "integer"},
            },
        },
    },
    "required": ["thread_id", "action"],
}


async def execute(
    thread_id: str,
    action: str,
    new_limits: Optional[Dict] = None,
    **params,
) -> Dict[str, Any]:
    """Resume a suspended thread, optionally with bumped limits.

    Actions:
    - resume: Resume with existing limits (only valid if limit wasn't the cause)
    - bump_and_resume: Apply new limits, then resume
    - cancel: Cancel the thread permanently
    """
    project_path = Path(params.get("_project_path", Path.cwd()))
    threads_dir = project_path / ".ai" / "threads"
    thread_dir = threads_dir / thread_id

    # Load thread metadata
    # Note: registry (SQLite) is the canonical source for thread status.
    # thread.json is a cached convenience copy. Always check/update both.
    meta_path = thread_dir / "thread.json"
    if not meta_path.exists():
        return {"success": False, "error": f"Thread not found: {thread_id}"}

    meta = json.loads(meta_path.read_text())
    # Prefer registry status over thread.json (registry is canonical)
    registry = ThreadRegistry(threads_dir / "registry.db")
    reg_status = registry.get_status(thread_id)
    status = reg_status.get("status", "") if reg_status else meta.get("status", "")

    if status != "suspended":
        return {
            "success": False,
            "error": f"Thread {thread_id} is not suspended (status: {status}). "
                     f"Only suspended threads can be resumed.",
        }

    if action == "cancel":
        return await _cancel_thread(thread_id, project_path, meta)

    # Load harness state
    state_path = thread_dir / "state.json"
    if not state_path.exists():
        return {
            "success": False,
            "error": f"No state.json for thread {thread_id}. Cannot resume without saved state.",
        }

    state = json.loads(state_path.read_text())

    # Apply new limits if bumping
    if action == "bump_and_resume" and new_limits:
        for key, value in new_limits.items():
            if key in state.get("limits", {}):
                old_value = state["limits"][key]
                state["limits"][key] = value
                logger.info(f"Bumped {key}: {old_value} → {value}")

        # Persist updated state
        save_state_dict(thread_id, state, project_path)

        # Also update budget ledger if spend was bumped
        if "spend" in new_limits:
            try:
                from budget_ledger import BudgetLedger
                ledger = BudgetLedger(threads_dir / "registry.db")
                ledger.update_max_spend(thread_id, new_limits["spend"])
            except Exception as e:
                logger.warning(f"Failed to update budget ledger: {e}")

    # Clear escalation file
    escalation_path = thread_dir / "escalation.json"
    if escalation_path.exists():
        escalation_path.unlink()

    # Load provider config for conversation reconstruction
    directive_name = meta.get("directive", "")
    model_config = meta.get("model", {})
    if isinstance(model_config, str):
        model_id = model_config
        provider_id = meta.get("provider", "rye/agent/providers/anthropic_messages")
    else:
        model_id = model_config.get("id", "claude-3-5-haiku-20241022")
        provider_id = model_config.get("provider", "rye/agent/providers/anthropic_messages")

    # Reconstruct conversation and resume LLM loop
    # This follows the same pattern as conversation_mode.continue_thread()
    from safety_harness import SafetyHarness
    from core_helpers import rebuild_conversation_from_transcript, run_llm_loop

    harness = SafetyHarness.from_state_dict(state, project_path)
    provider_config = _load_provider_config(provider_id, project_path)
    messages = rebuild_conversation_from_transcript(thread_id, project_path, provider_config)

    # Get tool definitions
    tool_defs = meta.get("tools", [])
    tool_map = {t.get("name", ""): t.get("item_id", "") for t in tool_defs if isinstance(t, dict)}

    # Update status to running
    meta["status"] = "running"
    meta_path.write_text(json.dumps(meta, indent=2))

    # Resume LLM loop
    transcript = TranscriptWriter(threads_dir)
    transcript.write_event(thread_id, "thread_resumed", {
        "directive": directive_name,
        "previous_status": status,
        "new_limits": new_limits or {},
        "cost_at_resume": harness.cost.to_dict(),
    })

    result = await run_llm_loop(
        project_path=project_path,
        model_id=model_id,
        provider_id=provider_id,
        provider_config=provider_config,
        tool_defs=tool_defs,
        tool_map=tool_map,
        harness=harness,
        messages=messages,
        max_tokens=state.get("limits", {}).get("tokens", 1024),
        directive_name=directive_name,
        thread_id=thread_id,
        transcript=transcript,
    )

    # Persist final state
    save_state(thread_id, harness, project_path)

    # Update status
    final_status = "completed" if result.get("success") else "error"
    meta["status"] = final_status
    meta["cost"] = harness.cost.to_dict()
    meta_path.write_text(json.dumps(meta, indent=2))

    return {
        "success": result.get("success", False),
        "status": final_status,
        "thread_id": thread_id,
        "cost": harness.cost.to_dict(),
        "text": result.get("text", ""),
    }

Note: `resume_thread` should also invoke signal_completion() for suspended threads
so any waiters are released even when the resume fails or is cancelled.
```

### Escalation from the User's Perspective

The approval file at `.ai/threads/{thread_id}/approvals/approval-{ts}.request.json` looks like:

```json
{
  "id": "approval-1739012650",
  "prompt": "Thread 'implement_feature' has reached its spend limit.\n\nCurrent spend: $0.9823\nLimit: $1.00\nTurns used: 8\nTokens used: 42,150\n\nApprove to increase spend limit to $2.00?",
  "thread_id": "implement_feature-1739012700",
  "created_at": "2026-02-12T10:00:00Z",
  "timeout_seconds": 3600
}
```

The user (or an automated policy) writes the response:

```json
{
  "approved": true,
  "message": "Approved, increase to $2.00",
  "new_limits": {
    "spend": 2.0
  }
}
```

Then calls:

```python
await resume_thread.execute(
    thread_id="implement_feature-1739012700",
    action="bump_and_resume",
    new_limits={"spend": 2.00},
)
```

The thread picks up exactly where it left off — same conversation history, same cost tracking (cumulative), new limits applied.

---

## 4. Crash Recovery and Orphan Detection

### Problem

If the process dies (OOM, SIGKILL, power loss) while a thread is running:

- Thread status stays "running" in the registry
- No state.json exists (currently) to resume from
- The thread is orphaned — nobody will ever update its status

With checkpoint-based persistence (section 1), we now have `state.json`. But we still need to detect orphans and offer recovery.

### Orphan Detection

A thread is considered orphaned if:

1. Status is "running" in the registry
2. No in-process asyncio task exists for it (process restarted)
3. The last transcript event is older than a threshold (e.g., 5 minutes for single-turn, longer for conversation mode)

```python
class OrphanDetector:
    """Detect and recover orphaned threads."""

    STALE_THRESHOLD_SECONDS = 300  # 5 minutes without activity

    def __init__(self, project_path: Path):
        self.project_path = project_path
        self.threads_dir = project_path / ".ai" / "threads"

    def scan(self) -> List[Dict[str, Any]]:
        """Scan for orphaned threads.

        Returns list of orphan info dicts with:
        - thread_id
        - directive
        - last_activity: ISO timestamp of last transcript event
        - age_seconds: seconds since last activity
        - has_state: whether state.json exists (resumable)
        - cost: cost at time of orphaning
        """
        registry = ThreadRegistry(self.threads_dir / "registry.db")
        running = registry.query(status="running")
        orphans = []

        for thread in running:
            thread_id = thread["thread_id"]

            # Check if there's an active in-process task
            from thread_tool import get_task
            if get_task(thread_id) is not None:
                continue  # Not orphaned — task is running

            # Check last transcript activity
            transcript_path = self.threads_dir / thread_id / "transcript.jsonl"
            last_activity = self._get_last_activity(transcript_path)

            if last_activity is None:
                age = float("inf")
            else:
                age = (datetime.now(timezone.utc) - last_activity).total_seconds()

            if age < self.STALE_THRESHOLD_SECONDS:
                continue  # Recent activity — probably still running

            # Check if state.json exists
            state_path = self.threads_dir / thread_id / "state.json"
            has_state = state_path.exists()

            # Load cost from state or thread.json
            cost = {}
            if has_state:
                try:
                    state = json.loads(state_path.read_text())
                    cost = state.get("cost", {})
                except Exception:
                    pass
            else:
                meta_path = self.threads_dir / thread_id / "thread.json"
                if meta_path.exists():
                    try:
                        meta = json.loads(meta_path.read_text())
                        cost = meta.get("cost", {})
                    except Exception:
                        pass

            orphans.append({
                "thread_id": thread_id,
                "directive": thread.get("directive_id", ""),
                "last_activity": last_activity.isoformat() if last_activity else None,
                "age_seconds": age if age != float("inf") else None,
                "has_state": has_state,
                "cost": cost,
                "recoverable": has_state,
            })

        return orphans

    def _get_last_activity(self, transcript_path: Path) -> Optional[datetime]:
        """Get timestamp of last event in transcript."""
        if not transcript_path.exists():
            return None

        last_ts = None
        try:
            with open(transcript_path, "r") as f:
                for line in f:
                    if not line.strip():
                        continue
                    try:
                        event = json.loads(line)
                        ts_str = event.get("ts") or event.get("timestamp")
                        if ts_str:
                            last_ts = datetime.fromisoformat(ts_str.replace("Z", "+00:00"))
                    except Exception:
                        continue
        except Exception:
            pass

        return last_ts

    def recover(self, thread_id: str, action: str = "resume") -> Dict[str, Any]:
        """Mark an orphan for recovery.

        Actions:
        - resume: Attempt to resume (requires state.json)
        - mark_error: Mark as errored (no resume)
        - mark_cancelled: Mark as cancelled
        """
        registry = ThreadRegistry(self.threads_dir / "registry.db")

        if action == "resume":
            state_path = self.threads_dir / thread_id / "state.json"
            if not state_path.exists():
                return {
                    "success": False,
                    "error": f"Cannot resume {thread_id}: no state.json. "
                             f"Use mark_error or mark_cancelled instead.",
                }
            # Mark as suspended so resume_thread can pick it up
            # suspend_reason="error" goes in state.json, not in the registry status
            registry.update_status(thread_id, "suspended")
            return {"success": True, "status": "suspended", "action": "ready_for_resume"}

        elif action == "mark_error":
            registry.update_status(thread_id, "error")
            return {"success": True, "status": "error"}

        elif action == "mark_cancelled":
            registry.update_status(thread_id, "cancelled")
            return {"success": True, "status": "cancelled"}

        return {"success": False, "error": f"Unknown action: {action}"}
```

### Startup Scan

When the orchestration system starts (or when a user runs a diagnostic tool), orphan detection runs automatically:

```python
async def execute(action: str = "scan", **params) -> Dict[str, Any]:
    """Orphan detection and recovery tool.

    Actions:
    - scan: List all orphaned threads
    - recover: Recover a specific orphan (requires thread_id + recovery_action)
    - auto_recover: Scan and auto-recover all orphans with state.json
    """
    project_path = Path(params.get("_project_path", Path.cwd()))
    detector = OrphanDetector(project_path)

    if action == "scan":
        orphans = detector.scan()
        return {
            "success": True,
            "orphans": orphans,
            "count": len(orphans),
            "recoverable": sum(1 for o in orphans if o["recoverable"]),
        }
    ...
```

---

## 5. Cancellation

### Problem

There's no way to stop a running thread from outside. If a thread is burning tokens on a bad plan, the user has to wait for it to hit a limit or crash.

### Design

Cancellation uses a **poison file** — a simple, race-free mechanism that doesn't require IPC. The tool-use loop checks for the existence of a cancel file at each checkpoint.

```
.ai/threads/{thread_id}/cancel.requested
```

The cancel tool must validate the thread_id against the registry and refuse to
write if the thread is not in `running` or `suspended` status to avoid arbitrary
poison-file injection. For extra hardening, store a random per-thread nonce in
thread.json and require the cancel tool to include it in the request payload.

### Cancel Tool

```python
async def execute(thread_id: str, reason: str = "", **params) -> Dict[str, Any]:
    """Request cancellation of a running thread.

    Creates a cancel.requested file that the thread's tool-use loop
    checks at each checkpoint. Non-blocking — returns immediately.
    """
    project_path = Path(params.get("_project_path", Path.cwd()))
    cancel_path = project_path / ".ai" / "threads" / thread_id / "cancel.requested"
    cancel_path.parent.mkdir(parents=True, exist_ok=True)

    cancel_data = {
        "requested_at": datetime.now(timezone.utc).isoformat(),
        "reason": reason,
    }

    tmp_path = cancel_path.with_suffix(".tmp")
    with open(tmp_path, "w") as f:
        json.dump(cancel_data, f)
    tmp_path.rename(cancel_path)

    return {
        "success": True,
        "thread_id": thread_id,
        "status": "cancel_requested",
    }
```

### Loop Integration

At each checkpoint in `_run_tool_use_loop`:

```python
# At top of each turn iteration:
cancel_path = project_path / ".ai" / "threads" / thread_id / "cancel.requested"
if cancel_path.exists():
    # Read cancel reason
    try:
        cancel_data = json.loads(cancel_path.read_text())
        reason = cancel_data.get("reason", "User cancelled")
    except Exception:
        reason = "User cancelled"

    # Clean up cancel file
    cancel_path.unlink(missing_ok=True)

    # Save state (so the work so far is recoverable)
    save_state(thread_id, harness, project_path)

    # Emit cancellation event
    if transcript:
        transcript.write_event(thread_id, "thread_cancelled", {
            "directive": directive_name,
            "reason": reason,
            "cost": harness.cost.to_dict(),
            "turn": turn,
        })

    return {
        "success": False,
        "error": f"Cancelled: {reason}",
        "cancelled": True,
    }
```

Cancellation is cooperative — the thread checks for the file and exits cleanly at the next checkpoint. Between checkpoints (e.g., during a long tool execution), the thread cannot be interrupted. This is intentional — it ensures state is always consistent.

> **opencode reference:** opencode uses `AbortController`/`AbortSignal` — the cancellation signal propagates through every async operation automatically. The poison-file approach is more durable (survives process crashes, works across unrelated processes) but trades immediacy. A hybrid: check the poison file at loop checkpoints (current design) AND pass an `asyncio.Event` to retry sleeps and managed subprocess waits so those can be interrupted without waiting for the next checkpoint.

For managed subprocesses, the cancel handler also stops any processes owned by the thread:

```python
# In cancel handler, before returning:
processes_dir = project_path / ".ai" / "threads" / thread_id / "processes"
if processes_dir.exists():
    from managed_subprocess import stop_all_processes
    stop_all_processes(thread_id, project_path)
```

---

## 6. Hierarchical Failure Propagation

### Problem

When a child thread in Wave 1 fails, the orchestrator doesn't discover it until `wait_threads` checks. With polling, this means up to 2 seconds of delay. More critically, if a child failure makes the entire plan invalid (e.g., the database schema child fails → API routes child's work is useless), the orchestrator needs to cancel the still-running sibling immediately — not after the next poll cycle.

### Design: Push-First, Audit-Second

Failure propagation uses a **push-based** primary mechanism and a **transcript-based** audit mechanism:

1. **Completion events** — each child thread sets an `asyncio.Event` on termination (success, error, suspension, or cancellation). `wait_threads` awaits these events directly, discovering failures with zero delay. When `fail_fast=true`, the first error triggers immediate sibling cancellation.

2. **Transcript audit trail** — the child also writes a `child_thread_failed` event to the parent's transcript. This is for post-hoc analysis, not coordination. The parent's LLM never reads this during execution — `wait_threads` handles coordination through completion events.

The key insight: **coordination signals flow through `asyncio.Event` (instant, in-process). Audit records flow through transcript JSONL (durable, cross-process).** Mixing the two — using transcript writes as a coordination mechanism — creates a system where the notification exists but nobody reads it during execution.

### Wait Threads — Fail Fast Mode

`wait_threads` now includes `fail_fast` and `cancel_siblings_on_failure` parameters (see [internals doc](thread-orchestration-internals.md#3-push-based-coordination-replacing-polling) for full implementation):

```python
CONFIG_SCHEMA = {
    ...
    "properties": {
        ...
        "fail_fast": {
            "type": "boolean",
            "description": "Return immediately when any thread errors",
            "default": false,
        },
        "cancel_siblings_on_failure": {
            "type": "boolean",
            "description": "Cancel remaining threads when one fails (requires fail_fast)",
            "default": false,
        },
    },
}
```

When `fail_fast=true`, `wait_threads` uses `asyncio.as_completed()` to react to each child as it finishes. The first error triggers:

1. Sibling cancellation via poison files (if `cancel_siblings_on_failure=true`)
2. Immediate return to the orchestrator LLM with error details
3. No wasted time waiting for siblings that will be cancelled anyway

```python
# In wait_threads — fail_fast path (simplified):
for coro in asyncio.as_completed(waiters, timeout=timeout):
    tid, result = await coro
    results[tid] = result

    if fail_fast and result.get("status") == "error":
        if cancel_siblings_on_failure:
            for sibling_id in thread_ids:
                if sibling_id != tid and sibling_id not in results:
                    cancel_path = project_path / ".ai" / "threads" / sibling_id / "cancel.requested"
                    cancel_path.parent.mkdir(parents=True, exist_ok=True)
                    tmp_path = cancel_path.with_suffix(".tmp")
                    tmp_path.write_text(json.dumps({
                        "requested_at": datetime.now(timezone.utc).isoformat(),
                        "reason": f"Sibling thread {tid} failed",
                    }))
                    tmp_path.rename(cancel_path)
                    results[sibling_id] = {"status": "cancelled", "reason": "sibling_failure"}
        return {"success": False, "failed_thread": tid, "threads": results}
```

### Parent Notification (Audit Trail)

When a child thread completes with an error, `thread_directive.execute()` writes a failure event to the parent's transcript. This is **not the coordination mechanism** — `wait_threads` handles that through completion events. The transcript write is an audit record:

```python
# In thread_directive.execute(), error path:
parent_thread_id = params.get("_parent_thread_id")
if parent_thread_id:
    try:
        parent_transcript = TranscriptWriter(project_path / ".ai" / "threads")
        parent_transcript.write_event(parent_thread_id, "child_thread_failed", {
            "child_thread_id": thread_id,
            "directive": directive_name,
            "error": llm_result.get("error", ""),
            "cost": harness.cost.to_dict(),
        })
    except Exception as e:
        logger.warning(f"Failed to write parent notification: {e}")
```

This ensures the parent's transcript has a record for debugging and replay, even if the `wait_threads` call hasn't happened yet. But the coordination path is: child sets completion event → `wait_threads` wakes up → orchestrator LLM receives result.

### Signal Flow

```
Child thread:
  execute() → error → finally block → signal_completion(thread_id)
                                              │
                                              ▼
                                     asyncio.Event.set()
                                              │
Parent (blocked in wait_threads):              │
  await event.wait() ◄────────────────────────┘
       │
       ▼
  fail_fast check → cancel siblings → return to LLM

Separately (audit):
  Child → transcript.write_event(parent_id, "child_thread_failed", {...})
  Parent transcript ← durable record for post-hoc analysis
```

---

## Complete Recovery Matrix

| Scenario                        | Detection                           | State Available               | Recovery Path                                                     |
| ------------------------------- | ----------------------------------- | ----------------------------- | ----------------------------------------------------------------- |
| **429 rate limit**              | Error classifier → RATE_LIMITED     | In-memory (still running)     | Auto-retry with Retry-After delay                                 |
| **503 / connection reset**      | Error classifier → TRANSIENT        | In-memory (still running)     | Auto-retry with exponential backoff (max 3)                       |
| **API quota exhausted**         | Error classifier → QUOTA            | state.json (checkpoint saved) | Auto-retry once after 60s, then suspend                           |
| **max_spend hit**               | check_limits() → spend_exceeded     | state.json (checkpoint saved) | Suspend → escalation request → user approves → bump_and_resume    |
| **max_turns hit**               | check_limits() → turns_exceeded     | state.json (checkpoint saved) | Suspend → escalation request → user approves → bump_and_resume    |
| **max_tokens hit**              | check_limits() → tokens_exceeded    | state.json (checkpoint saved) | Suspend → escalation request → user approves → bump_and_resume    |
| **Hierarchical budget**         | budget_ledger.check_remaining() ≤ 0 | state.json + ledger           | Suspend → parent notified → parent bumps budget or cancels        |
| **Process crash (OOM/SIGKILL)** | OrphanDetector.scan()               | state.json (if checkpointed)  | Orphan scan → mark suspended → resume_thread                      |
| **Process crash (no state)**    | OrphanDetector.scan()               | transcript.jsonl only         | Orphan scan → mark_error (manual intervention)                    |
| **User cancellation**           | cancel.requested file               | state.json (saved on cancel)  | Cancel → state preserved → optionally resume later                |
| **Child failure (in wave)**     | Completion event → wait_threads     | Child's state.json            | Immediate: fail_fast returns, siblings cancelled, parent re-plans |
| **Permanent error**             | Error classifier → PERMANENT        | state.json (checkpoint saved) | Thread errors → no retry → user reads transcript                  |

## Thread Status State Machine

```
                    ┌──────────────────────────────────────────────────┐
                    │                                                  │
                    ▼                                                  │
              ┌──────────┐                                            │
   start ───▶│ running   │◄────── resume_thread                      │
              └────┬─────┘                                            │
                   │                                                  │
        ┌──────────┼──────────────┬───────────────┐                   │
        │          │              │               │                   │
        ▼          ▼              ▼               ▼                   │
   ┌─────────┐ ┌────────────┐ ┌──────────────┐ ┌──────────────┐      │
   │completed│ │ error      │ │suspended     │ │ cancelled    │      │
   └─────────┘ └────────────┘ │ (reason in   │ └──────────────┘      │
                              │  state.json) │                        │
                              └──────────────┘────────────────────────┘
                                    │
                                    ├── approve → resume_thread → running
                                    └── deny → cancelled
```

## New Files Summary (Resilience Layer)

| File                                    | Purpose                                              |
| --------------------------------------- | ---------------------------------------------------- |
| `rye/agent/threads/error_classifier.py` | ErrorCategory enum, ERROR_PATTERNS, classify_error() |
| `rye/agent/threads/resume_thread.py`    | Resume suspended threads with optional limit bumps   |
| `rye/agent/threads/cancel_thread.py`    | Create cancel.requested poison file                  |
| `rye/agent/threads/orphan_detector.py`  | Scan for and recover orphaned threads                |

## Modified Files Summary (Resilience Layer)

| File                  | Changes                                                                                                            |
| --------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `thread_directive.py` | Add checkpoint saves in \_run_tool_use_loop, cancel file check, \_call_llm_with_retry, parent failure notification |
| `safety_harness.py`   | Add suspended statuses, \_handle_limit_hit for escalation flow                                                     |
| `thread_registry.py`  | Accept new statuses (suspended, cancelled)                                                                         |
| `wait_threads.py`     | Add fail_fast and cancel_siblings_on_failure params                                                                |
| `core_helpers.py`     | Same checkpoint saves in run_llm_loop                                                                              |

---

> For canonical definitions of thread statuses, tool names, hook events, and file locations, see [Canonical Vocabulary](thread-orchestration-internals.md#canonical-vocabulary).
