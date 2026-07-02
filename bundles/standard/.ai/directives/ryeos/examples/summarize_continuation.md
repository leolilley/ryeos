<!-- ryeos:signed:2026-07-02T03:44:33Z:ebf016cfab7b26cf4504e94d07ff7bb984bfdee41fed9cb86e7f18b21c37a454:WEZAPf9gVg+kMqNapWEIxw5ve/f6i7n1URu2tZB8reI58FHgNgXfKufGUjJ/XKO8pfiCTV8GbcYO10UMQkgHDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Example continuation hook target that summarizes the live context window for a successor directive run."
version: "1.0.0"
model:
  tier: fast
limits:
  turns: 2
  tokens: 12000
  spend_usd: 0.03
continuation: false
inputs:
  - name: reason
    type: string
    required: true
  - name: live_messages
    type: array
    required: true
  - name: usage
    type: object
    required: false
  - name: budget_remaining
    type: object
    required: false
  - name: declared_outputs
    type: object
    required: false
  - name: max_summary_tokens
    type: integer
    required: false
---

# Continuation Summary Example

You are preparing a compact continuation seed for a successor run of another directive.

The parent directive hit a context-window continuation boundary.

## Boundary

Reason: `{input:reason}`

Usage:

`{input:usage}`

Remaining budget:

`{input:budget_remaining}`

Declared outputs:

`{input:declared_outputs}`

Live provider-window messages:

`{input:live_messages}`

## Task

Return a concise continuation seed for the successor directive. Include:

1. Current objective.
2. Completed work.
3. Important decisions and assumptions.
4. Open tasks.
5. Critical facts, identifiers, file paths, URLs, tool results, or errors.
6. The recommended next action.

Keep the seed under `{input:max_summary_tokens}` tokens when that input is present.

Do not include generic advice. Preserve exact technical details the successor needs to continue without replaying the full context.
