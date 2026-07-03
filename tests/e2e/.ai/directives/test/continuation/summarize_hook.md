<!-- ryeos:signed:2026-07-02T05:15:26Z:b8d7bce577589825a6073556bee1f8a908e7929027ad6e803a3d503b11e1b334:EdvO9s5pB7o6DmSGZvOdG4DZIRHG+B5RHA3kRuuwvO99lx2imafIDE1iChg8Og1UOh8vhJsw3uBsIcS2z8+nAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Reason: `{input:reason}`

Live messages: `${inputs.live_messages|json}`

Usage: `${inputs.usage|json}`

Budget remaining: `${inputs.budget_remaining|json}`

Declared outputs: `${inputs.declared_outputs|json}`
