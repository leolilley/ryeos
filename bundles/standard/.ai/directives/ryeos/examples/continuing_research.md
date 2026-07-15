<!-- ryeos:signed:2026-07-02T03:44:33Z:05994c198c37c4d4378aa223f3b3f6dac032fdb726f3aee6c7d6565000d93c76:Kd2W12LBNOLKm4gPMR21RLtywpNpqQUYKPtqrsRWUD7odm4RkpeqiO6uYbO0K2YTP+zCYn5gD/QvfTAE10vRCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Example long-running directive that self-continues at the context boundary and seeds the successor with a summary hook."
version: "1.0.0"
model:
  tier: general
limits:
  turns: 80
  tokens: 400000
  spend_usd: 4.00
  depth: 5
continuation:
  carry_turns: 8
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.ryeos/examples/summarize_continuation
hooks:
  - id: summarize-before-continuing
    event: continuation
    action:
      item_id: directive:ryeos/examples/summarize_continuation
      thread: inline
      params:
        reason: ${event.reason}
        live_messages: ${event.messages}
        usage: ${event.usage}
        budget_remaining: ${event.budget_remaining}
        declared_outputs: ${event.declared_outputs}
        max_summary_tokens: 2000
---

# Continuing Research Example

This directive demonstrates the context-window continuation pattern:

1. Work normally until the live provider context approaches the configured context boundary.
2. The `continuation` event fires.
3. The hook calls `directive:ryeos/examples/summarize_continuation` inline.
4. The summary directive's result is injected into the successor run as runtime continuation context.
5. The successor continues this same directive with the last complete turns plus that seed.

## Task

Carry out the user's long-running research task thoroughly and incrementally.

## Operating rules

- Keep track of completed work, open questions, important facts, and next steps as you go.
- If resumed after a continuation boundary, treat the runtime-provided continuation seed as authoritative context from the previous segment.
- Do not restart completed work unless the continuation seed says the previous attempt was invalid.
- Preserve exact identifiers, file paths, URLs, errors, and decisions that are needed later.
- When the task is complete, give a concise final answer with the result and any remaining caveats.
