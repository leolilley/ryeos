# Data-Driven Unified Architecture

> The optimal consolidated config surface for the entire thread harness.
>
> Replaces the 10 individual data-driven docs with 7 configs across 3 domains + 2 schemas.

## Design Principles

1. **One owner per concern** — no config defines behavior that another config also defines
2. **Domain cohesion** — configs group by what changes together, not by implementation module
3. **Schema vs policy** — structural definitions (state.json shape, SQLite DDL) are separate from behavioral policy (when to checkpoint, how to retry)
4. **Project overridable** — every config supports `extends:` for project-level customization
5. **Audience clarity** — each config has one primary consumer (one Python class that loads it)

## File Structure

```
rye/rye/.ai/tools/rye/agent/threads/config/
│
│  ── 3 POLICY DOMAINS (behavioral — what the harness does) ──
│
├── runtime.yaml              # Domain 1: How threads execute
├── resilience.yaml           # Domain 2: How threads handle failure
├── security.yaml             # Domain 3: What threads are allowed to do
│
│  ── 2 INFRASTRUCTURE (structural — what exists) ──
│
├── events.yaml               # Event type registry (names, schemas, criticality)
├── streaming.yaml            # Transport layer (SSE, sinks, extraction)
│
│  ── 2 SCHEMAS (data definitions — no behavior) ──
│
├── state_schema.yaml         # state.json JSON Schema (structure only)
└── budget_ledger_schema.yaml # SQLite DDL + operations (structure only)
```

**7 files total.** Down from 10 current + 5 missing = 15.

## What Got Consolidated

| Old Config                        | → Goes Into                                                                | Rationale                                                                  |
| --------------------------------- | -------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| `events.yaml`                     | **`events.yaml`** (stays)                                                  | Event registry is infrastructure, not policy                               |
| `error_classification.yaml`       | **`resilience.yaml`**                                                      | Error patterns are resilience policy                                       |
| `hook_conditions.yaml`            | **`resilience.yaml`** (operators) + inline in each domain (built-in hooks) | Operators are shared; built-in hooks belong with the policy they implement |
| `coordination.yaml`               | **`runtime.yaml`**                                                         | Coordination is execution semantics                                        |
| `resilience.yaml`                 | **`resilience.yaml`** (stays, expanded)                                    | Already the right home                                                     |
| `streaming.yaml`                  | **`streaming.yaml`** (stays, scoped down)                                  | Transport only; emission policy moves to events.yaml                       |
| `state_schema.yaml`               | **`state_schema.yaml`** (stays, schema only)                               | Checkpoint triggers/retention move to resilience.yaml                      |
| `budget_ledger_schema.yaml`       | **`budget_ledger_schema.yaml`** (stays)                                    | Pure DDL                                                                   |
| `thread_modes.yaml`               | **`runtime.yaml`**                                                         | Modes are execution semantics                                              |
| ~~spawn policy~~ (missing)        | **`runtime.yaml`**                                                         | Spawn is execution                                                         |
| ~~dispatch policy~~ (missing)     | **`runtime.yaml`**                                                         | Dispatch is execution                                                      |
| ~~subprocess policy~~ (missing)   | **`runtime.yaml`**                                                         | Subprocess is execution                                                    |
| ~~capabilities~~ (missing)        | **`security.yaml`**                                                        | Capabilities are security                                                  |
| ~~context pressure~~ (missing)    | **`resilience.yaml`**                                                      | Context pressure is resilience                                             |
| ~~approval protocol~~ (missing)   | **`resilience.yaml`**                                                      | Approval is resilience flow                                                |
| ~~failure propagation~~ (missing) | **`runtime.yaml`**                                                         | Propagation is coordination policy                                         |

## Split-Brain Resolutions

| Conflict                                                                   | Resolution                                                                                                                                                                                                                            |
| -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Checkpoint triggers in both `resilience.yaml` and `state_schema.yaml`      | `resilience.yaml` owns triggers + retention. `state_schema.yaml` is schema only.                                                                                                                                                      |
| Event emission policy in both `events.yaml` and `streaming.yaml`           | `events.yaml` owns criticality + emission rules. `streaming.yaml` maps SSE chunks → event types only.                                                                                                                                 |
| Hook operators defined in `hook_conditions.yaml`                           | Operators move to `resilience.yaml` (they're the evaluation engine for resilience/hook policy). Built-in hooks distribute to the domain they implement.                                                                               |
| Batching config in both `streaming.yaml` (tool_parsing.batch) and dispatch | `runtime.yaml` owns dispatch batching. `streaming.yaml` owns parser batching (accumulation). These are different concerns: parser batching = "when is a tool definition complete", dispatch batching = "when to fire gathered tools". |

---

## Domain 1: `runtime.yaml` — How Threads Execute

**Owner:** `ThreadRuntime` class (new)
**Audience:** `thread_directive.py`, `spawn_thread.py`, `wait_threads.py`, `managed_subprocess.py`

This is the execution policy surface. Everything about how threads run, spawn, dispatch tools, manage subprocesses, coordinate, and propagate failures.

```yaml
# runtime.yaml
schema_version: "1.0.0"

# ─── THREAD MODES ───────────────────────────────────────────────
modes:
  single:
    description: "Execute directive once, complete"
    lifecycle: [running, completed, error, cancelled]
    resume_capability: false
    state_persistence: per_turn

  conversation:
    description: "Multi-turn with suspend/resume"
    lifecycle: [running, suspended, completed, error, cancelled]
    resume_capability: true
    state_persistence: per_turn

  channel:
    description: "Multi-party coordination channel"
    lifecycle: [running, suspended, completed, error, cancelled]
    resume_capability: true
    turn_protocols: [round_robin, on_demand]

# ─── CANONICAL VOCABULARY ───────────────────────────────────────
# Single source of truth for status names and terminality.
# Every other config references these — never defines its own.
vocabulary:
  thread_statuses: [running, completed, error, suspended, cancelled]
  terminal_statuses: [completed, error, cancelled]
  suspend_reasons: [limit, error, budget, approval]

  hook_events:
    [
      before_step,
      after_step,
      error,
      limit,
      after_complete,
      context_window_pressure,
    ]

# ─── SPAWN POLICY ───────────────────────────────────────────────
spawning:
  # Default execution mode when LLM calls spawn_thread
  default_mode: "async_task" # async_task | sync (block caller)

  # Concurrency limits
  max_concurrent_children: 20 # Per parent thread
  max_total_threads: 100 # Per process

  # What child threads must declare
  require_child_limits:
    - spend # Non-negotiable — no magic $0.50 default
    # - turns                          # Optional, uncomment to enforce
    # - tokens                         # Optional

  # What children inherit from parent
  inherit:
    hooks: true # Child gets parent's hooks (can override)
    sinks: true # Child streams to same sinks
    capability_token: true # Parent token propagated (attenuated)
    limits_strategy: "explicit_only" # explicit_only | inherit_and_clamp

  # Thread ID generation
  thread_id:
    strategy: "directive_timestamp" # directive_timestamp | uuid | custom
    # Produces: "{directive_name}-{unix_timestamp}"

  # Spawn queueing when at capacity
  backpressure:
    strategy: "reject" # reject | queue
    queue_max_size: 50
    queue_timeout_seconds: 300

# ─── TOOL DISPATCH ──────────────────────────────────────────────
dispatch:
  # Parallel execution policy
  parallel:
    enabled: true
    grouping: "item_id" # Group key: tool calls to same item_id run sequential
    max_concurrent_groups: 25 # Cap on concurrent groups per turn
    max_inflight_tools: 50 # Absolute cap across all groups
    preserve_input_order: true # Results returned in original call order

  # Dispatch batching (when to fire accumulated tool calls)
  # Different from streaming.tool_parsing.batch (which is about parsing completeness)
  batching:
    enabled: true
    max_batch_size: 5 # Fire immediately when N tools ready
    max_delay_ms: 100 # Or fire after this delay if fewer ready
    max_wait_for_first_ms: 5000 # Timeout waiting for first tool in batch

  # Per-tool timeouts
  timeouts:
    default_seconds: 300 # 5 minutes
    overrides: {} # tool_name → timeout_seconds

  # What happens when a single tool call errors during dispatch
  on_tool_error: "emit_and_continue" # emit_and_continue | fail_step | classify_and_retry

  # Progress reporting for long-running tools
  progress:
    enabled: true
    throttle_seconds: 1.0 # Max 1 progress event per second
    milestones: [0, 25, 50, 75, 100] # Emit at these percentages

# ─── MANAGED SUBPROCESS ────────────────────────────────────────
subprocess:
  # Resource limits
  limits:
    max_per_thread: 5 # Max managed processes per thread
    max_total: 20 # Max across all threads in process
    max_output_buffer_lines: 1000 # Ring buffer size

  # Lifecycle
  termination:
    sigterm_grace_seconds: 5 # Wait after SIGTERM
    sigkill_after_seconds: 10 # SIGKILL if still alive

  # Readiness detection
  readiness:
    default_timeout_seconds: 30
    behavior: "optional" # optional | required (fail if no ready signal)

  # Output capture
  logs:
    path_template: ".ai/threads/{thread_id}/processes/{handle_id}.log"
    max_file_mb: 10
    rotation:
      enabled: true
      max_files: 5
      compress_rotated: true

  # Security: command restrictions
  # Note: capability tokens provide the primary security layer.
  # This is defense-in-depth for subprocess specifically.
  command_policy:
    mode: "allow_all" # allow_all | allowlist | denylist
    allowlist: [] # Only when mode=allowlist
    denylist: [] # Only when mode=denylist

# ─── COORDINATION ──────────────────────────────────────────────
coordination:
  # wait_threads configuration
  wait_threads:
    default_timeout_seconds: 600 # 10 minutes
    max_timeout_seconds: 3600 # 1 hour hard cap
    min_timeout_seconds: 1

  # Completion event management
  completion_events:
    retention_minutes: 60 # Keep events in memory after completion
    cleanup_interval_minutes: 30
    max_tracked_events: 10000

  # Active task monitoring
  task_monitoring:
    enabled: true
    check_interval_seconds: 60
    hang_detection:
      enabled: true
      threshold_minutes: 30 # Warn if task runs longer than this

  # Child thread coordination
  child_coordination:
    default_wait_mode: "all" # all | any
    fail_fast:
      default: false
      cancel_siblings_on_failure: false

  # Failure propagation: what parent does when child fails
  # Keyed by child's error category (from error classification)
  failure_propagation:
    default: "fail_parent" # fail_parent | suspend_parent | continue | retry_child

    by_category:
      transient:
        action: "retry_child"
        max_attempts: 2
      permanent:
        action: "fail_parent"
      rate_limited:
        action: "retry_child"
        max_attempts: 3
        delay_source: "child_retry_policy"
      budget:
        action: "suspend_parent"
        suspend_reason: "budget"
      cancelled:
        action: "continue" # Sibling cancelled, not a parent error

    # When to cancel siblings
    sibling_cancellation:
      on_categories: [permanent, budget] # Only cancel siblings for these
      # transient/rate_limited: let siblings continue while child retries

# ─── PROVENANCE ─────────────────────────────────────────────────
# What to record per thread run for reproducibility
provenance:
  enabled: true
  record:
    config_hash: true # SHA256 of loaded config snapshot
    model_version: true # Model name + version from provider response
    tool_versions: true # __version__ from each executed tool
    directive_hash: true # SHA256 of directive content at execution time
    runtime_version: true # Rye OS version

# ─── QUALITY GATES ──────────────────────────────────────────────
# Conditions checked before thread can complete successfully
quality_gates:
  enabled: false # Opt-in per project

  gates: []
  # Example gates (uncomment or add in project override):
  # - name: "tests_pass"
  #   trigger: "before_complete"
  #   tool: "test_runner"
  #   params: {suite: "all"}
  #   on_failure: "suspend"            # suspend | fail | warn
  #
  # - name: "lint_clean"
  #   trigger: "before_complete"
  #   tool: "linter"
  #   params: {fix: false}
  #   on_failure: "warn"

# ─── TOOL CACHING ──────────────────────────────────────────────
tool_cache:
  enabled: false # Opt-in

  # Which tools can be cached
  cacheable_tools: [] # tool_name list; empty = none
  # Example: ["read_file", "dependency_graph", "static_analysis"]

  # Cache key composition
  key_components:
    - "tool_name"
    - "input_hash" # SHA256 of sorted input params
    - "commit_hash" # Current git commit (if available)

  ttl_minutes: 60
  max_entries: 1000
  storage: "memory" # memory | sqlite
```

---

## Domain 2: `resilience.yaml` — How Threads Handle Failure

**Owner:** `ThreadResilience` class (new, absorbs `ErrorClassifier` + `HookEvaluator`)
**Audience:** `safety_harness.py`, `thread_directive.py`, `resume_thread.py`, `orphan_detector.py`

Everything about error handling, retry, checkpoints, budgets, escalation, approval, recovery, cancellation, and context management.

```yaml
# resilience.yaml
schema_version: "1.0.0"

# ─── SHARED OPERATORS ──────────────────────────────────────────
# Used by both error classification patterns and hook conditions.
# Defined once here — the evaluation engine for all matching.
operators:
  eq: { description: "Equal", types: [string, number, boolean] }
  ne: { description: "Not equal", types: [string, number, boolean] }
  gt: { description: "Greater than", types: [number] }
  gte: { description: "Greater than or equal", types: [number] }
  lt: { description: "Less than", types: [number] }
  lte: { description: "Less than or equal", types: [number] }
  in: { description: "In list", types: [array] }
  contains: { description: "String contains substring", types: [string] }
  starts_with: { description: "String starts with", types: [string] }
  ends_with: { description: "String ends with", types: [string] }
  regex: { description: "Regex match", types: [string] }
  exists: { description: "Path exists in context", types: [any] }

combinators:
  all: { description: "All conditions must match", min_children: 1 }
  any: { description: "Any condition can match", min_children: 1 }
  not: { description: "Condition must NOT match", exact_children: 1 }

# ─── ERROR CLASSIFICATION ──────────────────────────────────────
# Evaluated in order, first match wins.
# Absorbs the former error_classification.yaml entirely.
error_classification:
  patterns:
    # Rate Limiting
    - id: "http_429"
      name: "rate_limited"
      category: "rate_limited"
      retryable: true
      match:
        any:
          - path: "status_code"
            op: "eq"
            value: 429
          - path: "error.type"
            op: "in"
            value: ["rate_limit_error", "RateLimitError"]
          - path: "error.message"
            op: "regex"
            value: "rate limit|too many requests|throttled"
      retry_policy:
        type: "use_header"
        header: "retry-after"
        fallback:
          type: "fixed"
          delay: 60.0

    - id: "rate_limit_overquota"
      name: "quota_exceeded"
      category: "quota"
      retryable: true
      match:
        any:
          - path: "error.code"
            op: "eq"
            value: "insufficient_quota"
          - path: "error.message"
            op: "regex"
            value: "quota|billing limit|credit"
      retry_policy:
        type: "fixed"
        delay: 3600.0
        max_retries: 3

    # Transient Network Errors
    - id: "network_timeout"
      name: "transient_timeout"
      category: "transient"
      retryable: true
      match:
        any:
          - path: "error.type"
            op: "in"
            value: ["TimeoutError", "ReadTimeout", "ConnectTimeout"]
          - path: "error.message"
            op: "regex"
            value: "timeout|timed out"
      retry_policy:
        type: "exponential"
        base: 2.0
        max: 30.0

    - id: "network_connection"
      name: "transient_connection"
      category: "transient"
      retryable: true
      match:
        any:
          - path: "error.type"
            op: "in"
            value: ["ConnectionError", "ConnectionResetError"]
          - path: "error.message"
            op: "regex"
            value: "connection reset|connection refused|network|socket"
      retry_policy:
        type: "exponential"
        base: 2.0
        max: 60.0

    - id: "http_5xx"
      name: "transient_server"
      category: "transient"
      retryable: true
      match:
        path: "status_code"
        op: "in"
        value: [500, 502, 503, 504]
      retry_policy:
        type: "exponential"
        base: 2.0
        max: 120.0

    # Permanent Errors
    - id: "auth_failure"
      name: "permanent_auth"
      category: "permanent"
      retryable: false
      match:
        any:
          - path: "status_code"
            op: "in"
            value: [401, 403]
          - path: "error.code"
            op: "in"
            value: ["authentication_error", "authorization_error"]

    - id: "not_found"
      name: "permanent_not_found"
      category: "permanent"
      retryable: false
      match:
        path: "status_code"
        op: "eq"
        value: 404

    - id: "validation_error"
      name: "permanent_validation"
      category: "permanent"
      retryable: false
      match:
        any:
          - path: "status_code"
            op: "eq"
            value: 422
          - path: "error.type"
            op: "eq"
            value: "ValidationError"

    # Limit Events
    - id: "limit_spend"
      name: "limit_spend_exceeded"
      category: "limit_hit"
      retryable: false
      match:
        path: "limit_code"
        op: "eq"
        value: "spend_exceeded"
      action: "escalate"

    - id: "limit_turns"
      name: "limit_turns_exceeded"
      category: "limit_hit"
      retryable: false
      match:
        path: "limit_code"
        op: "eq"
        value: "turns_exceeded"
      action: "escalate"

    - id: "limit_tokens"
      name: "limit_tokens_exceeded"
      category: "limit_hit"
      retryable: false
      match:
        path: "limit_code"
        op: "eq"
        value: "tokens_exceeded"
      action: "escalate"

    # Budget Events
    - id: "budget_hierarchical"
      name: "budget_exhausted"
      category: "budget"
      retryable: false
      match:
        path: "error.code"
        op: "eq"
        value: "hierarchical_budget_exceeded"
      action: "escalate"

    # Cancellation
    - id: "cancelled"
      name: "cancelled"
      category: "cancelled"
      retryable: false
      match:
        any:
          - path: "error.type"
            op: "eq"
            value: "CancelledError"
          - path: "cancelled"
            op: "eq"
            value: true
      action: "abort"

  # Default when no pattern matches
  default:
    category: "permanent"
    retryable: false

  # Category descriptions
  categories:
    transient: "Retry with backoff"
    rate_limited: "Retry with rate-limit header"
    quota: "Retry with long fixed delay"
    permanent: "Fail immediately"
    limit_hit: "Escalate to user approval"
    budget: "Escalate to parent budget bump"
    cancelled: "Abort immediately"

# ─── RETRY POLICIES ────────────────────────────────────────────
retry:
  max_retries: 3 # Global cap

  policies:
    exponential:
      type: exponential
      base: 2.0
      multiplier: 2.0
      max_delay: 120.0
      formula: "min(base * (multiplier ** attempt), max_delay)"

    fixed:
      type: fixed
      delay: 60.0

    rate_limited:
      type: use_header
      header: "retry-after"
      fallback:
        policy: exponential
        base: 5.0

    quota_exceeded:
      type: fixed
      delay: 3600.0

  # Per-category retry rules
  rules:
    transient: { retryable: true, policy: exponential, max_retries: 3 }
    rate_limited: { retryable: true, policy: rate_limited, max_retries: 5 }
    quota: { retryable: true, policy: quota_exceeded, max_retries: 2 }
    permanent: { retryable: false }
    limit_hit: { retryable: false, action: escalate }
    budget: { retryable: false, action: escalate }
    cancelled: { retryable: false, action: abort }

# ─── CHECKPOINTS ────────────────────────────────────────────────
# Single authority for checkpoint triggers and retention.
# state_schema.yaml defines the JSON structure only.
checkpoint:
  triggers:
    pre_turn: { enabled: true, priority: 1 }
    post_llm: { enabled: true, priority: 2 }
    post_tools: { enabled: true, priority: 3 }
    on_suspend: { enabled: true, priority: 10 }
    on_error: { enabled: true, priority: 10 }
    on_cancel: { enabled: true, priority: 10 }

  persistence:
    format: "json"
    compression: false
    atomic_write: true # .tmp → rename

  retry:
    enabled: true
    max_attempts: 3
    backoff: "exponential"

  retention:
    max_checkpoints: 10
    cleanup:
      enabled: true
      keep_last_n: 3
      delete_older_than_days: 7
    on_completion: "archive" # archive | delete | keep
    archive_path: ".ai/threads/{thread_id}/archive/"

# ─── BUDGET & LIMITS ───────────────────────────────────────────
budget:
  defaults:
    spend: 1.0 # $1.00
    turns: 10
    tokens: 100000
    spawns: 5
    duration_minutes: 30

  hierarchical:
    enabled: true
    reservation_mode: "pessimistic" # Reserve full child budget upfront

  escalation:
    enabled: true
    proposal:
      strategy: "double" # proposed = current * 2
      max_multiplier: 10 # Cap at 10x original
    max_escalations: 3 # Per thread lifetime
    reset_counters_on_escalation: true

# ─── APPROVAL PROTOCOL ─────────────────────────────────────────
# How limit escalation requests are persisted and resolved.
approval:
  persistence:
    type: "file" # file | sqlite
    path_template: ".ai/threads/{thread_id}/escalation.json"
    atomic_write: true

  request_schema:
    required:
      [
        type,
        thread_id,
        directive,
        limit_code,
        current_value,
        current_max,
        proposed_max,
        message,
        approval_request_id,
      ]

  timeout_minutes: 60
  auto_approve_threshold: 5.0 # Auto-approve if proposed_max < this
  on_timeout: "keep_suspended" # keep_suspended | fail

  # Resume behavior after approval
  resume:
    prefer_checkpoint: true # Use state.json over transcript replay
    replay_events: [tool_call_result, cognition_out]
    max_replay_events: 1000
    partial_cognition_handling:
      include_in_prompt: true
      annotate: true # Add "[Stream interrupted: ...]" note

# ─── CONTEXT MANAGEMENT ────────────────────────────────────────
context_management:
  pressure:
    enabled: true
    threshold: 0.8 # Emit event when tokens_used/max_tokens > this
    hysteresis: 0.05 # Only re-fire after dropping below threshold - hysteresis
    cooldown_turns: 1 # Min turns between pressure events

  measurement:
    token_source: "provider_usage" # provider_usage | estimate
    max_tokens_source: "model_config" # Read from provider/model config

  # Default behavior when context_window_pressure fires and no hook matches
  default_action: "noop" # noop | call_directive
  # default_directive: "rye/agent/threads/default_compaction"

# ─── CANCELLATION ───────────────────────────────────────────────
cancellation:
  poison_file:
    name: "cancel.requested"
    schema: { required: [requested_at, reason] }

  graceful_shutdown:
    enabled: true
    timeout_seconds: 30
    cleanup:
      stop_managed_processes: true
      close_sinks: true
      save_final_state: true

# ─── ORPHAN DETECTION & RECOVERY ───────────────────────────────
recovery:
  orphan_detection:
    enabled: true
    check_interval_minutes: 5
    criteria:
      status_stuck_for_minutes: 10
      no_heartbeat_for_minutes: 5
      process_not_running: true

  options:
    auto_resume: false # Require user confirmation
    max_recovery_attempts: 3

  reconstruction:
    from_transcript: true
    from_checkpoint: true # Prefer checkpoint if available
    max_events_to_replay: 1000

# ─── BUILT-IN HOOKS ────────────────────────────────────────────
# Default hooks that ship with the system.
# These use the operators/combinators defined above.
# Directive-level hooks override these (higher priority wins).
builtin_hooks:
  - id: "default_limit_escalation"
    event: "limit"
    priority: 0 # Low priority — user hooks override
    condition:
      path: "event.limit_code"
      op: "exists"
    action:
      type: "escalate"
      target: "user_approval"

  - id: "default_transient_retry"
    event: "error"
    priority: 0
    condition:
      path: "classification.category"
      op: "eq"
      value: "transient"
    action:
      type: "retry"
      max_attempts: 3

  - id: "default_compaction"
    event: "context_window_pressure"
    priority: 0
    condition:
      path: "event.pressure_ratio"
      op: "gt"
      value: 0.8
    action:
      type: "call_directive"
      directive: "rye/agent/threads/default_compaction"
      parameters:
        pressure_ratio: "${event.pressure_ratio}"
        tokens_used: "${event.tokens_used}"
        max_tokens: "${event.max_tokens}"

# ─── HOOK ACTION TYPES ─────────────────────────────────────────
# Valid actions that hooks can return.
hook_action_types:
  retry:
    {
      parameters:
        {
          max_attempts: { type: integer, default: 3 },
          backoff_policy:
            {
              type: string,
              enum: [exponential, fixed, inherit],
              default: inherit,
            },
        },
    }
  fail: { parameters: { error_message: { type: string, optional: true } } }
  abort: { parameters: {} }
  continue: { parameters: {} }
  escalate:
    {
      parameters:
        {
          target:
            { type: string, enum: [user_approval, parent_notification, both] },
          timeout_seconds: { type: integer, default: 3600 },
        },
    }
  call_directive:
    {
      parameters:
        {
          directive: { type: string, required: true },
          parameters: { type: object, default: {} },
          inherit_token: { type: boolean, default: true },
        },
    }
  suspend:
    {
      parameters:
        {
          suspend_reason:
            {
              type: string,
              enum: [limit, error, budget, approval],
              required: true,
            },
        },
    }
  emit_event:
    {
      parameters:
        {
          event_type: { type: string, required: true },
          payload: { type: object, default: {} },
        },
    }
```

---

## Domain 3: `security.yaml` — What Threads Are Allowed to Do

**Owner:** `ThreadSecurity` class (new)
**Audience:** `thread_directive.py` (token minting), `spawn_thread.py` (token propagation), `PrimitiveExecutor` (enforcement), `managed_subprocess.py` (command policy)

```yaml
# security.yaml
schema_version: "1.0.0"

# ─── CAPABILITY TOKEN MODEL ────────────────────────────────────
capability_tokens:
  # Token format
  format: "json" # json | signed_json (future)

  # Minting rules
  minting:
    root_source: "directive_permissions" # Root threads mint from declared <permissions>
    # Future: ttl_minutes, audience restrictions

  # Delegation rules (parent → child)
  delegation:
    mode: "attenuate" # attenuate (intersection) | passthrough
    allow_wildcards: true # e.g., "rye.execute.tool.*"
    require_subset: true # Child caps must be ⊆ parent caps

  # Enforcement
  enforcement:
    on_missing_token:
      root_thread: "self_mint" # Root threads with no parent can self-mint
      child_thread: "deny" # Children without token → permission_denied
    on_denied: "permission_denied" # Return {status: "permission_denied", error: "..."}

# ─── CAPABILITY CATALOG ────────────────────────────────────────
# Canonical capability names and their descriptions.
# Tools/directives reference these in their <permissions> declarations.
capability_catalog:
  # Core execution
  "rye.execute.tool.*": "Execute any tool"
  "rye.execute.directive.*": "Execute any directive"

  # Thread operations
  "rye.thread.spawn": "Spawn child threads"
  "rye.thread.wait": "Wait for child thread completion"
  "rye.thread.cancel": "Cancel threads"
  "rye.thread.resume": "Resume suspended threads"

  # Subprocess
  "rye.subprocess.start": "Start managed subprocesses"
  "rye.subprocess.stop": "Stop managed subprocesses"

  # File system
  "rye.fs.read": "Read files"
  "rye.fs.write": "Write files"
  "rye.fs.delete": "Delete files"

  # Git
  "rye.git.commit": "Create git commits"
  "rye.git.branch": "Create/switch git branches"

  # Network
  "rye.net.http": "Make HTTP requests"

# ─── VERIFIED LOADER ───────────────────────────────────────────
verified_loader:
  enabled: true

  # Trust roots: directories where signed files can be loaded from
  trust_roots:
    - "${project_path}/.ai/tools"
    - "${user_space}/tools"
    - "${system_space}/tools"

  # Symlink policy
  symlinks: "deny" # deny | follow_if_within_roots

  # What happens on verification failure
  on_failure: "reject" # reject | warn | log_only

  # File types that require verification before load
  # (All types under trust roots are verified — this is documentation, not a filter)
  verified_extensions: [".py", ".js", ".sh", ".yaml", ".yml", ".json", ".toml"]

# ─── TOOL SANDBOXING ───────────────────────────────────────────
# Defense-in-depth restrictions beyond capability tokens.
sandbox:
  # File access boundaries
  filesystem:
    allowed_roots:
      - "${project_path}"
      - "${project_path}/.ai"
    deny_patterns:
      - "**/.env"
      - "**/.env.*"
      - "**/secrets/**"
      - "**/*.pem"
      - "**/*.key"

  # Network restrictions
  network:
    mode: "allow_all" # allow_all | allowlist | denylist
    allowlist: []
    denylist: []

  # Secrets handling
  secrets:
    redact_in_transcript: true # Scrub known secret patterns from transcript
    redact_patterns:
      - "sk-[a-zA-Z0-9]{20,}" # OpenAI/Anthropic API keys
      - "ghp_[a-zA-Z0-9]{36}" # GitHub PATs
      - "AKIA[0-9A-Z]{16}" # AWS access keys
```

---

## Infrastructure: `events.yaml` — Event Type Registry

**Owner:** `EventEmitter` class
**Audience:** Everything that emits or reads transcript events

**Change from current:** This is now the SOLE authority on event criticality and emission rules. `streaming.yaml` no longer defines emission policy.

```yaml
# events.yaml
schema_version: "1.0.0"

# ─── CRITICALITY LEVELS ────────────────────────────────────────
criticality_levels:
  critical:
    description: "Synchronous, blocking write. Thread waits for completion."
    durability: guaranteed
    async: false

  droppable:
    description: "Fire-and-forget async emission. May be dropped under pressure."
    durability: best_effort
    async: true
    fallback: drop

# ─── EVENT CATEGORIES ──────────────────────────────────────────
categories:
  lifecycle: "Thread start/stop/resume events"
  execution: "LLM turn events"
  cognition: "Input/output/reasoning events"
  tool: "Tool call events"
  error: "Error handling events"
  orchestration: "Child thread events"
  compaction: "Context window management"
  provenance: "Reproducibility records"

# ─── EVENT TYPE DEFINITIONS ────────────────────────────────────
event_types:
  # Lifecycle
  thread_started:
    category: lifecycle
    criticality: critical
    payload_schema:
      type: object
      required: [directive, model, provider]
      properties:
        directive: { type: string }
        model: { type: string }
        provider: { type: string }
        inputs: { type: object }
        thread_mode: { type: string, enum: [single, conversation, channel] }

  thread_completed:
    category: lifecycle
    criticality: critical
    payload_schema:
      type: object
      required: [cost]
      properties:
        cost:
          type: object
          properties:
            turns: { type: integer }
            tokens: { type: integer }
            spend: { type: number }
            duration_seconds: { type: number }

  thread_suspended:
    category: lifecycle
    criticality: critical
    payload_schema:
      type: object
      required: [suspend_reason]
      properties:
        suspend_reason: { type: string, enum: [limit, error, budget, approval] }
        cost: { type: object }

  thread_resumed:
    category: lifecycle
    criticality: critical
    payload_schema:
      type: object
      properties:
        resumed_by: { type: string }
        previous_suspend_reason: { type: string }

  thread_cancelled:
    category: lifecycle
    criticality: critical
    payload_schema:
      type: object
      properties:
        cancelled_by: { type: string }
        reason: { type: string }

  # Execution
  step_start:
    category: execution
    criticality: critical
    payload_schema:
      type: object
      required: [turn_number]
      properties:
        turn_number: { type: integer }

  step_finish:
    category: execution
    criticality: critical
    payload_schema:
      type: object
      required: [cost, tokens, finish_reason]
      properties:
        cost: { type: number }
        tokens:
          type: object
          properties:
            input_tokens: { type: integer }
            output_tokens: { type: integer }
        finish_reason:
          { type: string, enum: [end_turn, tool_use, limit_exceeded, error] }

  # Cognition
  cognition_in:
    category: cognition
    criticality: critical
    payload_schema:
      type: object
      required: [text, role]
      properties:
        text: { type: string }
        role: { type: string, enum: [system, user, developer] }

  cognition_out:
    category: cognition
    criticality: critical
    emit_on_error: true # Always emit partial text even if stream fails
    payload_schema:
      type: object
      required: [text]
      properties:
        text: { type: string }
        model: { type: string }
        is_partial: { type: boolean, default: false }
        truncated: { type: boolean, default: false }
        error: { type: string }
        completion_percentage: { type: number, minimum: 0, maximum: 100 }

  cognition_out_delta:
    category: cognition
    criticality: droppable
    emission:
      throttle: "1s"
      max_queue_size: 1000
      drop_policy: "oldest"
      condition: "provider_config.stream.enabled"
    payload_schema:
      type: object
      required: [text, chunk_index]
      properties:
        text: { type: string }
        chunk_index: { type: integer }
        is_final: { type: boolean }

  cognition_reasoning:
    category: cognition
    criticality: droppable
    emit_on_error: true # Accumulate and emit partial reasoning
    emission:
      accumulate_on_error: true
      condition: "provider_config.stream.enabled"
    payload_schema:
      type: object
      required: [text]
      properties:
        text: { type: string }
        is_partial: { type: boolean, default: false }
        was_interrupted: { type: boolean, default: false }

  # Tool events
  tool_call_start:
    category: tool
    criticality: critical
    payload_schema:
      type: object
      required: [tool, call_id, input]
      properties:
        tool: { type: string }
        call_id: { type: string }
        input: { type: object }

  tool_call_progress:
    category: tool
    criticality: droppable
    emission:
      throttle: "1s"
      milestones: [0, 25, 50, 75, 100]
    payload_schema:
      type: object
      required: [call_id, progress]
      properties:
        call_id: { type: string }
        progress: { type: number, minimum: 0, maximum: 100 }
        message: { type: string }

  tool_call_result:
    category: tool
    criticality: critical
    payload_schema:
      type: object
      required: [call_id, output]
      properties:
        call_id: { type: string }
        output: { type: string }
        error: { type: string }
        duration_ms: { type: integer }

  # Error & Recovery
  error_classified:
    category: error
    criticality: critical
    payload_schema:
      type: object
      required: [error_code, category]
      properties:
        error_code: { type: string }
        category:
          {
            type: string,
            enum:
              [
                transient,
                permanent,
                rate_limited,
                quota,
                limit_hit,
                budget,
                cancelled,
              ],
          }
        retryable: { type: boolean }
        metadata: { type: object }

  retry_succeeded:
    category: error
    criticality: critical
    payload_schema:
      type: object
      required: [original_error, retry_count]
      properties:
        original_error: { type: string }
        retry_count: { type: integer }
        total_delay_ms: { type: integer }

  limit_escalation_requested:
    category: error
    criticality: critical
    payload_schema:
      type: object
      required: [limit_code, current_value, proposed_max]
      properties:
        limit_code:
          {
            type: string,
            enum:
              [
                turns_exceeded,
                tokens_exceeded,
                spend_exceeded,
                spawns_exceeded,
                duration_exceeded,
              ],
          }
        current_value: { type: number }
        current_max: { type: number }
        proposed_max: { type: number }
        message: { type: string }
        approval_request_id: { type: string }

  # Orchestration
  child_thread_started:
    category: orchestration
    criticality: critical
    payload_schema:
      type: object
      required: [child_thread_id, child_directive]
      properties:
        child_thread_id: { type: string }
        child_directive: { type: string }
        parent_thread_id: { type: string }

  child_thread_failed:
    category: orchestration
    criticality: critical
    payload_schema:
      type: object
      required: [child_thread_id, error]
      properties:
        child_thread_id: { type: string }
        error: { type: string }

  # Compaction
  context_compaction_start:
    category: compaction
    criticality: critical
    payload_schema:
      type: object
      properties:
        triggered_by: { type: string }
        pressure_ratio: { type: number }

  context_compaction_end:
    category: compaction
    criticality: critical
    payload_schema:
      type: object
      properties:
        summary: { type: string }
        prune_before_turn: { type: integer }

  # Provenance (recorded at thread start for reproducibility)
  provenance_snapshot:
    category: provenance
    criticality: critical
    payload_schema:
      type: object
      properties:
        config_hash: { type: string }
        model_version: { type: string }
        directive_hash: { type: string }
        runtime_version: { type: string }
        tool_versions: { type: object }
```

---

## Infrastructure: `streaming.yaml` — Transport Layer

**Owner:** `StreamTransport` class
**Audience:** HTTP primitive, sink management

**Change from current:** No longer defines emission criticality (that's in `events.yaml`). Only defines how SSE chunks map to event types and how sinks work.

```yaml
# streaming.yaml
schema_version: "1.0.0"

# ─── HTTP TRANSPORT ─────────────────────────────────────────────
http:
  enabled: true
  mode: "stream" # stream | sync
  transport: "sse"

  connection:
    timeout: 120
    read_timeout: 60
    retry_on_disconnect: true
    max_reconnects: 3

  sse:
    event_prefix: "data:"
    ignore_empty_lines: true
    ignore_comments: true

# ─── SINKS ──────────────────────────────────────────────────────
sinks:
  enabled: true

  global:
    max_concurrent_writes: 100
    write_timeout_seconds: 5
    buffer_on_backpressure: true
    buffer_max_size: 10000

  definitions:
    websocket_ui:
      type: "websocket"
      enabled: true
      url: "${THREAD_UI_WEBSOCKET_URL:-ws://localhost:8080/events}"
      reconnect:
        attempts: 3
        backoff: "exponential"
        base_delay: 0.5
        max_delay: 30.0
      buffer:
        on_disconnect: true
        max_size: 1000
        drop_policy: "oldest"

    file_audit:
      type: "file"
      enabled: true
      path: ".ai/threads/{thread_id}/sse-raw.jsonl"
      format: "jsonl"
      rotation:
        enabled: true
        max_size_mb: 10
        max_files: 5
        compress_rotated: true

    return_sink:
      type: "return"
      enabled: true
      buffer_size: 10000

# ─── SSE → EVENT EXTRACTION ────────────────────────────────────
# Maps raw SSE chunks to thread event types.
# Does NOT define criticality (events.yaml owns that).
extraction:
  rules:
    text_delta:
      match: { path: "$.type", value: "content_block_delta" }
      extract: { text: "$.delta.text", index: "$.index" }
      emit_as: "cognition_out_delta"

    reasoning:
      match: { path: "$.type", value: "thinking" }
      extract: { text: "$.thinking" }
      emit_as: "cognition_reasoning"

    tool_use_start:
      match: { path: "$.type", value: "tool_use" }
      extract: { tool_id: "$.id", tool_name: "$.name", tool_input: "$.input" }
      emit_as: "cognition_in"

    tool_use_delta:
      match: { path: "$.type", value: "tool_use_delta" }
      extract: { partial_input: "$.partial_json" }
      accumulate: { by: "$.id", until: "tool_use_stop" }

    completion:
      match: { path: "$.type", value: "message_stop" }
      extract: { stop_reason: "$.stop_reason", usage: "$.usage" }
      emit_as: "step_finish"

# ─── TOOL PARSING ──────────────────────────────────────────────
# StreamingToolParser: accumulate partial tool defs from stream chunks.
# This is PARSER batching (when is a tool complete).
# DISPATCH batching is in runtime.yaml.
tool_parsing:
  enabled: true

  formats:
    xml:
      enabled: true
      tag_open: "<tool_use>"
      tag_close: "</tool_use>"
      parse_strategy: "accumulate_until_close"

    json:
      enabled: true
      schema:
        type: object
        required: [type, name]
        properties:
          type: { const: "tool_use" }
          id: { type: string }
          name: { type: string }
          input: { type: object }
      parse_strategy: "incremental_json"

  limits:
    max_tool_size: 1048576 # 1MB per tool definition
    max_concurrent_tools: 50
    max_text_buffer: 10485760 # 10MB total text

# ─── PROVIDER OVERRIDES ────────────────────────────────────────
providers:
  anthropic:
    http:
      url: "https://api.anthropic.com/v1/messages"
      headers:
        anthropic-version: "2023-06-01"
    extraction:
      rules:
        text_delta:
          extract: { text: "$.delta.text" }
        reasoning:
          match: { path: "$.type", value: "thinking" }

  openai:
    http:
      url: "https://api.openai.com/v1/chat/completions"
    extraction:
      rules:
        text_delta:
          extract: { text: "$.choices[0].delta.content" }
        tool_use:
          match: { path: "$.choices[0].delta.tool_calls", exists: true }

# ─── ERROR HANDLING ─────────────────────────────────────────────
error_handling:
  on_interruption:
    emit_partial_cognition: true
    emit_partial_reasoning: true
    preserve_completed_tools: true

  partial_cognition:
    required_fields: [text, is_partial]
    estimate_completion: true
```

---

## Schema: `state_schema.yaml` — Structure Only

**Change from current:** Checkpoint triggers and retention REMOVED (they live in `resilience.yaml`). This file is purely the JSON Schema for `state.json`.

```yaml
# state_schema.yaml
schema_version: "1.0.0"

# Pure structural definition for .ai/threads/{thread_id}/state.json
# Behavioral policy (when to save, retention, etc.) is in resilience.yaml
state:
  required: [thread_id, directive, version, saved_at]

  schema:
    type: object
    required: [thread_id, directive, version, saved_at]
    properties:
      thread_id: { type: string }
      directive: { type: string }
      parent_thread_id: { type: [string, "null"], default: null }
      version: { type: string, default: "1.0.0" }
      saved_at: { type: string, format: date-time }

      inputs: { type: object }
      turn_number: { type: integer, default: 0 }

      cost:
        type: object
        properties:
          turns: { type: integer, default: 0 }
          tokens:
            type: object
            properties:
              input_tokens: { type: integer, default: 0 }
              output_tokens: { type: integer, default: 0 }
          spend: { type: number, default: 0.0 }
          duration_seconds: { type: number, default: 0.0 }

      limits:
        type: object
        properties:
          spend: { type: [number, "null"] }
          turns: { type: [integer, "null"] }
          tokens: { type: [integer, "null"] }
          spawns: { type: [integer, "null"] }

      status:
        {
          type: string,
          enum: [running, suspended, completed, error, cancelled],
        }
      suspend_reason:
        { type: [string, "null"], enum: [limit, error, budget, approval, null] }
      suspend_metadata:
        type: [object, "null"]
        properties:
          limit_code:
            {
              type: string,
              enum:
                [
                  turns_exceeded,
                  tokens_exceeded,
                  spend_exceeded,
                  spawns_exceeded,
                  duration_exceeded,
                ],
            }
          current_value: { type: number }
          current_max: { type: number }

      hooks:
        type: array
        items:
          type: object
          properties:
            event: { type: string }
            directive: { type: string }
            inputs: { type: object }

      required_caps:
        type: array
        items: { type: string }

      messages:
        type: array
        items:
          type: object
          properties:
            role: { type: string, enum: [system, user, assistant] }
            content: { type: string }

      partial_cognition:
        type: [object, "null"]
        properties:
          text: { type: string }
          is_partial: { type: boolean }
          completion_percentage: { type: number }

      # Provenance snapshot (recorded at thread start)
      provenance:
        type: [object, "null"]
        properties:
          config_hash: { type: string }
          model_version: { type: string }
          directive_hash: { type: string }
```

---

## Schema: `budget_ledger_schema.yaml` — Unchanged

Stays as-is. Pure SQLite DDL + operations. No behavioral policy.

---

## Python Consumer Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  thread_directive.py (orchestrator)                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ThreadRuntime ─────► runtime.yaml                               │
│    .spawn()         (modes, spawn, dispatch, subprocess,         │
│    .dispatch()       coordination, failure propagation,          │
│    .wait()           provenance, quality gates, tool cache)      │
│                                                                  │
│  ThreadResilience ──► resilience.yaml                            │
│    .classify()      (errors, retry, checkpoints, budget,         │
│    .retry()          escalation, approval, context mgmt,         │
│    .checkpoint()     cancellation, recovery, hooks)              │
│    .evaluate_hook()                                              │
│                                                                  │
│  ThreadSecurity ────► security.yaml                              │
│    .mint_token()    (capability tokens, verified loader,         │
│    .verify_dep()     sandbox, secrets redaction)                  │
│    .check_caps()                                                 │
│                                                                  │
│  EventEmitter ──────► events.yaml                                │
│    .emit()          (event types, criticality, schemas,           │
│    .emit_droppable()  emission rules — SOLE authority)           │
│                                                                  │
│  StreamTransport ───► streaming.yaml                             │
│    .connect()       (HTTP/SSE, sinks, extraction,                │
│    .parse_tools()    tool parsing — transport only)              │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Config Loader

```python
# config_loader.py
class ThreadConfigLoader:
    """Load thread configs with 3-tier precedence:
    1. System defaults: rye/rye/.ai/tools/rye/agent/threads/config/*.yaml
    2. User space: ~/.ai/config/thread_*.yaml (optional)
    3. Project space: .ai/config/thread_*.yaml (optional)
    """

    @classmethod
    def load_runtime(cls, project_path: Path) -> RuntimeConfig: ...

    @classmethod
    def load_resilience(cls, project_path: Path) -> ResilienceConfig: ...

    @classmethod
    def load_security(cls, project_path: Path) -> SecurityConfig: ...

    @classmethod
    def load_events(cls, project_path: Path) -> EventConfig: ...

    @classmethod
    def load_streaming(cls, project_path: Path) -> StreamingConfig: ...

    @classmethod
    def load_state_schema(cls, project_path: Path) -> StateSchemaConfig: ...

    @classmethod
    def load_budget_schema(cls, project_path: Path) -> BudgetSchemaConfig: ...
```

## Completeness Verification

Every capability from the implementation plan has exactly one config home:

| Phase     | Capability                           | Config Owner                                                                 |
| --------- | ------------------------------------ | ---------------------------------------------------------------------------- |
| Pre-phase | Vocabulary alignment                 | `runtime.yaml` (vocabulary section)                                          |
| A0        | Canonical statuses/events            | `runtime.yaml` (vocabulary) + `events.yaml` (event types)                    |
| A1        | wait_threads                         | `runtime.yaml` (coordination)                                                |
| A2        | Async spawn                          | `runtime.yaml` (spawning)                                                    |
| A3        | Parallel dispatch                    | `runtime.yaml` (dispatch)                                                    |
| A4        | Capability tokens                    | `security.yaml` (capability_tokens)                                          |
| A5        | Budget ledger                        | `budget_ledger_schema.yaml` + `resilience.yaml` (budget)                     |
| A6        | Managed subprocess                   | `runtime.yaml` (subprocess)                                                  |
| B1        | Checkpoints                          | `resilience.yaml` (checkpoint) + `state_schema.yaml` (schema)                |
| B2        | Error classification                 | `resilience.yaml` (error_classification)                                     |
| B3        | Retry + streaming + context pressure | `resilience.yaml` (retry, context_management) + `streaming.yaml` (transport) |
| B4        | Limit escalation                     | `resilience.yaml` (escalation, approval)                                     |
| B5        | Resume thread                        | `resilience.yaml` (approval.resume)                                          |
| B6        | Cancellation                         | `resilience.yaml` (cancellation)                                             |
| B7        | Orphan recovery                      | `resilience.yaml` (recovery)                                                 |
| B8        | Failure propagation                  | `runtime.yaml` (coordination.failure_propagation)                            |
| C1        | Verified loader                      | `security.yaml` (verified_loader)                                            |
| C2        | Bundler                              | Tool CONFIG_SCHEMA (not system config — tool-level)                          |
| C3        | Node runtime                         | Tool CONFIG_SCHEMA (not system config — tool-level)                          |
| C4        | Orchestrator directives              | Directive XML metadata (not system config)                                   |
| D0-D3     | Bundle registry                      | `services/registry-api/` (server config, not thread config)                  |

## What Stays as Tool CONFIG_SCHEMA (Not System YAML)

These are tool-specific, not system-wide policy:

- **Bundler tool** — `action`, `bundle_id`, `version` (tool params, not policy)
- **Node runtime** — `command`, `args`, `timeout` (tool params)
- **Managed subprocess** — `action`, `handle_id`, `command` (tool params; system _limits_ are in `runtime.yaml`)

The distinction: **system YAML** defines policy defaults and constraints. **Tool CONFIG_SCHEMA** defines the tool's input interface. A tool may read system YAML for limits (`runtime.yaml.subprocess.limits`) but its own params are CONFIG_SCHEMA.
