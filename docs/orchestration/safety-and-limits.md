```yaml
id: safety-and-limits
title: "Safety and Limits"
description: Cost controls, turn limits, and the SafetyHarness
category: orchestration
tags: [safety, limits, budget, cost, harness]
version: "1.1.0"
```

# Safety and Limits

Every thread runs inside a `SafetyHarness` that enforces limits, evaluates hooks, and checks permissions. The harness is not an execution engine — it's a guard rail that the runner checks before and after each turn.

## Limit Types

| Limit              | Unit     | What it controls |
|--------------------|----------|-----------------|
| `turns`            | integer  | Maximum LLM conversation turns |
| `tokens`           | integer  | Maximum total tokens (input + output) |
| `spend`            | float    | Maximum USD spend for this thread |
| `duration_seconds` | float    | Maximum wall-clock execution time |
| `spawns`           | integer  | Maximum child threads this thread can spawn |
| `depth`            | integer  | Maximum remaining nesting depth |

## How Limits Resolve

Limits are resolved through a four-layer merge. Each layer can add or override values, and parent limits provide a hard ceiling:

```
resilience.yaml defaults → directive metadata → limit_overrides → parent upper bounds
```

### Layer 1: Defaults from `resilience.yaml`

Project-level defaults in `.ai/config/agent/resilience.yaml`:

```yaml
limits:
  defaults:
    turns: 15
    tokens: 200000
    spend: 0.50
    spawns: 10
    depth: 5
    duration_seconds: 600
```

### Layer 2: Directive metadata

The directive's XML `<limits>` element overrides defaults:

```xml
<limits max_turns="30" max_tokens="200000" />
```

### Layer 3: `limit_overrides` parameter

The spawning parent can override limits when calling `execute directive`:

```python
rye_execute(
    item_type="directive",
    item_id="agency-kiwi/leads/discover_leads",
    limit_overrides={"turns": 10, "spend": 0.10}
)
```

### Layer 4: Parent upper bounds

After merging the first three layers, parent limits cap every value via `min()`:

```python
for key in ("turns", "tokens", "spend", "spawns", "duration_seconds"):
    if key in parent_limits and key in resolved:
        resolved[key] = min(resolved[key], parent_limits[key])
```

A child can never exceed its parent. If the parent has `spend: 1.00` and the child requests `spend: 5.00`, the child gets `spend: 1.00`.

### Depth Decrement

Depth is special — it decrements by 1 per level instead of using `min()`:

```python
if "depth" in parent_limits:
    resolved["depth"] = min(resolved.get("depth", 10), parent_limits["depth"] - 1)
```

If the parent has `depth: 3`, the child gets `depth: 2`, its children get `depth: 1`, and their children get `depth: 0`. At depth ≤ 0, thread creation fails with "Depth limit exhausted". This prevents infinite spawn recursion.

### Resolution Example

```
resilience.yaml:  turns=15, spend=0.50, depth=5
directive XML:    turns=30              (overrides to 30)
limit_overrides:  turns=10, spend=0.10  (overrides to 10 and 0.10)
parent limits:    turns=30, spend=1.00, depth=4

Result:           turns=10, spend=0.10, depth=3
                  (turns: min(10,30)=10, spend: min(0.10,1.00)=0.10, depth: min(10,4-1)=3)
```

## Limit Checking

The runner calls `harness.check_limits(cost)` at the start of every turn:

```python
def check_limits(self, cost: Dict) -> Optional[Dict]:
    checks = [
        ("turns", cost.get("turns", 0), self.limits.get("turns")),
        ("tokens", cost.get("input_tokens", 0) + cost.get("output_tokens", 0),
         self.limits.get("tokens")),
        ("spend", cost.get("spend", 0.0), self.limits.get("spend")),
        ("duration_seconds", cost.get("elapsed_seconds", 0),
         self.limits.get("duration_seconds")),
    ]
    for limit_code, current, maximum in checks:
        if maximum is not None and current >= maximum:
            return {
                "limit_code": f"{limit_code}_exceeded",
                "current_value": current,
                "current_max": maximum,
            }
    return None
```

When a limit is exceeded:

1. The limit event is passed to `harness.run_hooks("limit", event, ...)` — hooks can handle it (e.g., log a warning, trigger a handoff)
2. If a hook returns an action, that action determines the thread's fate
3. If no hook handles it, the thread terminates with a fail-safe error: `"Limit exceeded: turns_exceeded (10/10)"`

## The Budget Ledger

The `BudgetLedger` is a SQLite-backed hierarchical cost tracking system at `.ai/agent/threads/budget_ledger.db`. It ensures threads can't overspend their allocation.

### Registration

Root threads register their budget ceiling:

```python
ledger.register(thread_id, max_spend=3.00)
```

### Reservation

Child threads atomically reserve budget from their parent. `BEGIN IMMEDIATE` serializes concurrent reservations so two children can't over-commit the parent's remaining budget:

```python
ledger.reserve(child_thread_id, amount=0.10, parent_thread_id="parent-123")
```

The ledger checks: `remaining = max_spend - actual_spend - sum(active children's reserved_spend)`. If the child's requested amount exceeds `remaining`, the reservation fails with `InsufficientBudget`.

### Spend Reporting

After the LLM loop finishes, the actual spend is reported:

```python
ledger.report_actual(thread_id, actual_spend=0.07)
```

If actual spend exceeds the reservation, `BudgetOverspend` is raised (logged but doesn't block finalization).

### Cascade

The child's actual spend is added to the parent's `actual_spend`:

```python
ledger.cascade_spend(child_thread_id, parent_thread_id, actual_spend=0.07)
```

This means the parent's `actual_spend` reflects the total cost of its entire subtree — its own LLM calls plus all children's costs.

### Release

On completion, the reservation is set to actual spend (freeing unused budget back to the parent):

```python
ledger.release(thread_id, final_status="completed")
# Sets reserved_spend = actual_spend
# Parent's remaining budget increases by (old_reserved - actual_spend)
```

### Budget Flow Example

```
Root orchestrator: max_spend = $3.00

  1. Register root:        reserved=$3.00, actual=$0.00, remaining=$3.00
  2. Root uses 2 turns:    reserved=$3.00, actual=$0.15, remaining=$2.85
  3. Spawn child A ($0.10): remaining=$2.85 → reserve $0.10 → remaining=$2.75
  4. Spawn child B ($0.10): remaining=$2.75 → reserve $0.10 → remaining=$2.65
  5. Child A completes:    actual=$0.07, cascade $0.07 to root
     Root: actual=$0.22, child A released ($0.03 freed), remaining=$2.68
  6. Child B completes:    actual=$0.09, cascade $0.09 to root
     Root: actual=$0.31, child B released ($0.01 freed), remaining=$2.69
```

### Querying Budget

```python
# Get remaining budget for a thread
remaining = ledger.get_remaining(thread_id)

# Check if a spawn is affordable before attempting it
check = ledger.can_spawn(parent_thread_id, requested_budget=0.10)
# {"affordable": True, "remaining": 2.69, "requested": 0.10}

# Get total spend across entire subtree
tree = ledger.get_tree_spend(thread_id)
# {"total_actual": 0.31, "total_reserved": 3.00, "thread_count": 3, "active_count": 0}
```

## Hooks System

Hooks provide event-driven behavior during thread execution. They are evaluated by the `SafetyHarness` when specific events occur.

### Hook Sources and Layers

| Layer | Source | Config Location | Purpose |
|-------|--------|-----------------|---------|
| 0 | User hooks | `~/.ai/config/agent/hooks.yaml` | Cross-project personal hooks |
| 1 | Directive hooks | Directive XML `<hooks>` block | Per-directive hooks |
| 2 | Builtin hooks | System `hook_conditions.yaml` | Error/limit/compaction defaults |
| 3 | Project hooks | `.ai/config/agent/hooks.yaml` | Project-wide hooks |
| 4 | Infra hooks | System `hook_conditions.yaml` | Infrastructure (emitter, checkpoint) |

Hooks from all five sources are merged and sorted by layer. User and project hooks use the same format as directive hooks — `id`, `event`, optional `condition`, and `action`. For control flow events (`error`, `limit`, `after_step`), the first hook that returns a non-None action wins — except layer 4 (infra) hooks which always execute regardless.

### Hook Events

| Event | When | Purpose |
|-------|------|---------|
| `thread_started` | Before first LLM turn | Load context (knowledge items, identity) |
| `limit` | When any limit is exceeded | Handle limit violations |
| `error` | When LLM call fails | Error classification and retry logic |
| `after_step` | After each turn completes | Logging, cost tracking, custom logic |
| `context_limit_reached` | When context window is nearly full | Trigger handoff |

### Hook Dispatch

**Control hooks** (`run_hooks`): For error/limit/after_step events. Returns a control action (retry, terminate, etc.) or None (continue).

**Context hooks** (`run_hooks_context`): For `thread_started` and `thread_continued` events. Runs ALL matching hooks, concatenates their context strings. Used to inject knowledge (agent identity, project rules) into the first message.

### Example: Directive with Hooks

```xml
<hooks>
  <hook>
    <when>cost.current > cost.limit * 0.9</when>
    <execute item_type="directive">warn-cost-critical</execute>
  </hook>
  <hook>
    <when>error.type == "permission_denied"</when>
    <execute item_type="directive">request-elevated-permissions</execute>
    <inputs>
      <requested_resource>${error.resource}</requested_resource>
    </inputs>
  </hook>
</hooks>
```

## Limits in Practice

### Execution Leaf (Simple)

```xml
<limits max_turns="4" max_tokens="4096" />
```

Spawned with: `limit_overrides: {"turns": 4, "spend": 0.05}`

A `score_lead` leaf that calls one scoring tool and returns. 4 turns is enough: first turn calls the tool, second turn processes the result. Spend is capped at $0.05 — haiku is cheap.

### Sub-Orchestrator (Moderate)

```xml
<limits max_turns="20" max_tokens="200000" />
```

Spawned with: `limit_overrides: {"turns": 20, "spend": 1.00}`

A `qualify_leads` sub-orchestrator needs more turns to spawn children, wait for them, aggregate results, and save output. The $1.00 budget covers its own reasoning plus all child spawns.

### Root Orchestrator (Complex)

```xml
<limits max_turns="30" max_tokens="200000" />
```

Spawned with: `limit_overrides: {"turns": 30, "spend": 3.00}`

The root `run_lead_pipeline` needs the most turns for multi-phase coordination and the largest budget to cover the entire tree of children and grandchildren.

## Capability Risk Enforcement

In addition to turns, tokens, and spend limits, threads are subject to **capability risk classification** at setup time. This is a separate safety layer — risk classification is orthogonal to the limit system above.

### How It Works

The `_assess_capability_risk` function runs once during thread setup (in `thread_directive.py`), after the `SafetyHarness` is initialized and capabilities are resolved. It checks every granted capability against `capability_risk.yaml`:

1. For each capability, find the most-specific matching classification pattern
2. Look up the risk tier (`safe`, `write`, `elevated`, `unrestricted`)
3. Apply the tier's policy (`allow`, `acknowledge_required`, `block`)

If any capability is classified as `block` and not acknowledged, the thread fails immediately — before the first LLM turn. This is a hard stop at setup time, not a per-turn check like limits.

### Broad Capability Warnings

When capabilities use broad patterns like `rye.*` or `rye.execute.*`, the risk classifier logs warnings even if the policy allows execution:

```
WARNING: Thread abc123: capability 'rye.execute.*' classified as 'elevated' —
Broad execute grants access to all tools and directives.
Consider adding <acknowledge risk="elevated"> to the directive.
```

These warnings help authors notice when a directive has more access than intended. They don't block execution (unless the policy is `block`).

### Relationship to Limits

| Safety Layer | When It Runs | What It Controls | Failure Mode |
| ------------ | ------------ | ---------------- | ------------ |
| Limits (turns, tokens, spend) | Every turn | Resource consumption | Thread terminates after exhaustion |
| Capability permissions | Every tool call | Which tools/knowledge are accessible | Tool call denied, LLM sees error |
| Capability risk | Thread setup (once) | Whether dangerous capabilities are acknowledged | Thread refuses to start |

Risk classification and limits serve different purposes and don't interact — a thread can pass risk checks but hit a spend limit, or have safe capabilities but exhaust its turns. Both systems enforce independently.

See [Permissions and Capabilities — Capability Risk Classification](./permissions-and-capabilities.md#capability-risk-classification) for the full risk model and `<acknowledge>` syntax.

## What's Next

- [Permissions and Capabilities](./permissions-and-capabilities.md) — How capability tokens control what threads can do
- [Continuation and Resumption](./continuation-and-resumption.md) — What happens when context limits are reached
