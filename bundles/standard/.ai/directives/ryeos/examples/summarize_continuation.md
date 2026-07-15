<!-- ryeos:signed:2026-07-15T07:49:16Z:89eb5c301b432f907445a87da16d15de28de44e39a86ec8ede526c6285b16ba0:+XRsTU/ZtlWK9fyZx5scQxlzF23IvkRkKUHPBM54e/J+HwnE8BXjx8aUhmfu4LnfQFxGX3TC1sNn+o7H20IlBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Reason: `${inputs.reason}`

Usage:

`${json(inputs.usage ?? null)}`

Remaining budget:

`${json(inputs.budget_remaining ?? null)}`

Declared outputs:

`${json(inputs.declared_outputs ?? null)}`

Live provider-window messages:

`${json(inputs.live_messages)}`

## Task

Return a concise continuation seed for the successor directive. Include:

1. Current objective.
2. Completed work.
3. Important decisions and assumptions.
4. Open tasks.
5. Critical facts, identifiers, file paths, URLs, tool results, or errors.
6. The recommended next action.

When `${exists(inputs.max_summary_tokens)}` is true, keep the seed under
`${inputs.max_summary_tokens ?? 0}` tokens.

Do not include generic advice. Preserve exact technical details the successor needs to continue without replaying the full context.
