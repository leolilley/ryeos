<!-- rye:signed:2026-02-26T06:42:50Z:575f1954970b639ed22cd5d1dde7389fd4c8a7f6546cd61602825b8a1febb481:baoVZjxW8w4ZV8Xr8D1oF2sMYDxdiqWplqrTNYWy1gaHcJxb5jo89hCkm_I1tD-_McibNrsTyxvBvazC1PzIBA==:4b987fd4e40303ac -->

```yaml
name: limits-and-safety
title: Limits and Safety
entry_type: reference
category: rye/agent/threads
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - limits
  - safety
  - hooks
  - cost
references:
  - thread-lifecycle
  - permissions-in-threads
  - "docs/orchestration/safety-and-limits.md"
```

# Limits and Safety

Every thread runs inside a `SafetyHarness` that enforces limits, evaluates hooks, and checks permissions. The harness is a guard rail — the runner checks it before and after each turn.

## Limit Types

| Limit              | Unit    | Controls                              |
|--------------------|---------|---------------------------------------|
| `turns`            | integer | Maximum LLM conversation turns        |
| `tokens`           | integer | Maximum total tokens (input + output) |
| `spend`            | float   | Maximum USD spend for this thread     |
| `duration_seconds` | float   | Maximum wall-clock execution time     |
| `spawns`           | integer | Maximum child threads spawnable       |
| `depth`            | integer | Maximum remaining nesting depth       |

## Limit Resolution Order

Four-layer merge. Each layer can add or override. Parent limits are a hard ceiling.

```
resilience.yaml defaults → directive metadata → limit_overrides → parent upper bounds
```

### Layer 1: Defaults (`resilience.yaml`)

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

```xml
<limits max_turns="30" max_tokens="200000" />
```

### Layer 3: `limit_overrides` parameter

Passed by the spawning parent via `execute directive` (which delegates to `thread_directive` internally):

```python
"limit_overrides": {"turns": 10, "spend": 0.10}
```

### Layer 4: Parent upper bounds

After merging layers 1–3, parent limits cap every value via `min()`:

```python
for key in ("turns", "tokens", "spend", "spawns", "duration_seconds"):
    if key in parent_limits and key in resolved:
        resolved[key] = min(resolved[key], parent_limits[key])
```

**A child can never exceed its parent.** Parent `spend: 1.00` + child requests `spend: 5.00` → child gets `spend: 1.00`.

### Depth Decrement

Depth decrements by 1 per level (not `min()`):

```python
if "depth" in parent_limits:
    resolved["depth"] = min(resolved.get("depth", 10), parent_limits["depth"] - 1)
```

Parent `depth: 3` → child `depth: 2` → grandchild `depth: 1` → great-grandchild `depth: 0` → **fails with "Depth limit exhausted"**.

### Resolution Example

```
resilience.yaml:  turns=15, spend=0.50, depth=5
directive XML:    turns=30              (overrides to 30)
limit_overrides:  turns=10, spend=0.10  (overrides to 10 and 0.10)
parent limits:    turns=30, spend=1.00, depth=4

Result:           turns=10, spend=0.10, depth=3
```

## Limit Checking

Runner calls `harness.check_limits(cost)` at start of every turn:

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
            return {"limit_code": f"{limit_code}_exceeded",
                    "current_value": current, "current_max": maximum}
    return None
```

When exceeded:
1. Limit event passed to `harness.run_hooks("limit", event, ...)`
2. Hook can return a control action
3. If no hook handles it → **terminate with `"Limit exceeded: turns_exceeded (10/10)"`**

## Budget Ledger

SQLite-backed hierarchical cost tracking at `.ai/agent/threads/budget_ledger.db`.

### Operations

| Operation           | Method                                      | Description                                |
|---------------------|---------------------------------------------|--------------------------------------------|
| Register (root)     | `ledger.register(thread_id, max_spend)`     | Create top-level budget entry              |
| Reserve (child)     | `ledger.reserve(child_id, amount, parent_id)` | Atomic reservation from parent's remaining |
| Report actual       | `ledger.report_actual(thread_id, spend)`    | Record actual spend after loop completes   |
| Cascade to parent   | `ledger.cascade_spend(child_id, parent_id, spend)` | Add child spend to parent's actual  |
| Release             | `ledger.release(thread_id, status)`         | Set reserved = actual, free unused budget  |
| Check affordability | `ledger.can_spawn(parent_id, amount)`       | Pre-check before spawning                  |
| Get remaining       | `ledger.get_remaining(thread_id)`           | Current remaining budget                   |
| Get tree spend      | `ledger.get_tree_spend(thread_id)`          | Total spend across entire subtree          |

### Reservation Serialization

`BEGIN IMMEDIATE` serializes concurrent reservations — two children can't over-commit the parent's remaining budget.

Remaining = `max_spend - actual_spend - sum(active children's reserved_spend)`.

If requested > remaining → `InsufficientBudget` error, child never starts.

### Budget Flow Example

```
Root: max_spend = $3.00
  1. Register:        reserved=$3.00, actual=$0.00, remaining=$3.00
  2. Root 2 turns:    reserved=$3.00, actual=$0.15, remaining=$2.85
  3. Spawn A ($0.10): remaining=$2.85 → reserve → remaining=$2.75
  4. Spawn B ($0.10): remaining=$2.75 → reserve → remaining=$2.65
  5. A completes:     actual=$0.07, cascade → root actual=$0.22
                      Release A ($0.03 freed) → remaining=$2.68
  6. B completes:     actual=$0.09, cascade → root actual=$0.31
                      Release B ($0.01 freed) → remaining=$2.69
```

### Overspend Handling

If actual spend > reservation → `BudgetOverspend` raised (logged but doesn't block finalization).

## Hook System

Hooks provide event-driven behavior during thread execution, evaluated by `SafetyHarness`.

### Hook Layers

| Layer | Source | Location | Purpose |
|-------|--------|----------|---------|
| 0 | User hooks | `~/.ai/config/agent/hooks.yaml` | Cross-project personal hooks |
| 1 | Directive hooks | Directive XML `<hooks>` block | Per-directive hooks |
| 2 | Builtin hooks | System `hook_conditions.yaml` | Error/limit/compaction defaults |
| 3 | Project hooks | `.ai/config/agent/hooks.yaml` | Project-wide hooks |
| 4 | Infra hooks | System `hook_conditions.yaml` | Infrastructure (emitter, checkpoint) |

### Hook Events

| Event                    | When                              | Purpose                           |
|--------------------------|-----------------------------------|-----------------------------------|
| `thread_started`         | Before first LLM turn            | Load context (identity, rules)    |
| `limit`                  | Any limit exceeded                | Handle limit violations           |
| `error`                  | LLM call fails                    | Error classification and retry    |
| `after_step`             | After each turn completes         | Logging, cost tracking            |
| `context_limit_reached`  | Context window nearly full        | Trigger handoff                   |

### Hook Dispatch Types

- **Control hooks** (`run_hooks`): For error/limit/after_step. Returns control action (retry, terminate) or None (continue). First non-None wins (except layer 4 infra hooks always run).
- **Context hooks** (`run_hooks_context`): For `thread_started` and `thread_continued` events. Runs ALL matching hooks, concatenates context strings.

### Control Actions

| Action      | Effect                                      |
|-------------|---------------------------------------------|
| `continue`  | Proceed normally                            |
| `retry`     | Retry the failed operation                  |
| `terminate` | Stop the thread with the provided message   |

## Practical Limit Presets

| Role             | Turns | Spend  | Tokens  | Why                                 |
|------------------|-------|--------|---------|-------------------------------------|
| Execution leaf   | 4     | $0.05  | 4096    | One tool call and return            |
| Strategy leaf    | 6     | $0.05  | 4096    | Load knowledge, decide, return      |
| Execution leaf+  | 10    | $0.10  | 4096    | Tool call with state management     |
| Sub-orchestrator | 20    | $1.00  | 200000  | Spawn/wait/aggregate cycle          |
| Root orchestrator| 30    | $3.00  | 200000  | Multi-phase coordination            |

## tool_result_guard

Large tool results are bounded before being appended to the conversation:

- Results exceeding the configured size threshold are truncated
- Duplicate results are deduplicated
- Large artifacts are stored externally and replaced with references
