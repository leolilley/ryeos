<!-- ryeos:signed:2026-11-11T09:41:52Z:cb91e3e77e4b137db6be2c664afe6cefad8ff6457ee0e0753352896180bfcd35:bqhixCepUwasvqidY/YsZ/cHAABBl+iomaPl6GVhCxJT85MCT7YRGDbt6/7uXTnYg29kZtKBVxCkETOZN8biAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "E2E continuation hook target that summarizes the live continuation boundary."
version: "1.0.0"
model:
  provider: zen
  name: gpt-5.4-nano
  context_window: 128000
limits:
  turns: 2
  tokens: 12000
  spend_usd: 0.02
continuation: false
inputs:
  - name: reason
    type: string
    required: false
  - name: live_messages
    type: array
    required: false
  - name: usage
    type: object
    required: false
  - name: budget_remaining
    type: object
    required: false
  - name: declared_outputs
    type: array
    required: false
  - name: max_summary_tokens
    type: integer
    required: false
---

# E2E Continuation Summary Hook

Return exactly one short sentence beginning with `CONTINUATION_HOOK_SUMMARY:`.

Reason: `${inputs.reason ?? "unspecified"}`

Live messages: `${json(inputs.live_messages ?? [])}`

Usage: `${json(inputs.usage ?? null)}`

Budget remaining: `${json(inputs.budget_remaining ?? null)}`

Declared outputs: `${json(inputs.declared_outputs ?? null)}`
