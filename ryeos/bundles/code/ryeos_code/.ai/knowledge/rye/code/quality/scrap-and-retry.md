<!-- rye:signed:2026-03-16T09:53:45Z:57375b6c05186ccd181d777882c4306d5c1e01db9ab0fb9783e1746e85de5bf3:lt7WuIJNyZPlw9U80IGSoHrkfhQRzgOuN39Wc9dYUhX2o1mQs-kbZtHuep-T7NjC7mhwfeFP_VnTvhNC96uTCw==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: scrap-and-retry
title: Scrap and Retry Philosophy
entry_type: reference
category: rye/code/quality
version: "1.0.0"
author: rye-os
created_at: 2026-03-04T00:00:00Z
tags:
  - quality
  - anti-slop
  - orchestration
  - retry
```

# Scrap and Retry

Never fix bad output. Discard it, fix the root cause, and rerun from scratch.

## The Principle

When an agent produces low-quality output — wrong approach, poor structure, slop patterns, failing tests — do not attempt to patch it. The agent that produced bad output will produce bad patches for the same reasons. Instead:

1. **Diagnose** — Identify why the output was bad. Missing context? Wrong directive? Bad input data? Ambiguous requirements?
2. **Discard** — Throw away the entire output. Do not salvage partial results.
3. **Fix** — Address the root cause. Inject missing context, clarify the directive, fix the input, narrow the scope.
4. **Rerun** — Execute the directive again from scratch with the fix applied.

## Why Not Patch

- Patching bad output compounds errors. Each fix introduces new assumptions that may conflict with the original bad assumptions.
- The agent's context is already polluted by the bad reasoning that led to the bad output. Asking it to fix its own work means reasoning on top of flawed reasoning.
- A clean rerun with the root cause fixed produces better results in fewer total tokens than iterative patching.

## For Orchestrators

When coordinating build-then-review cycles:

- If the review rejects the output, do not send the rejection feedback to the same thread. That thread's context is contaminated.
- Spawn a new thread with the original directive plus the review feedback injected as additional context.
- Cap the retry count. If the same directive fails 3 times with different root-cause fixes, the problem is in the directive or the requirements — escalate to the user.
- Each retry should include: the original task, the review verdict from the previous attempt, and the specific root-cause fix applied. The new thread should understand what went wrong without seeing the bad output itself.

## When to Escalate Instead

Not every failure is retriable. Escalate when:

- The directive itself is ambiguous or contradictory.
- The required information does not exist in the codebase or provided context.
- The task requires capabilities the agent does not have (e.g., needs write access but only has read-only).
- The same failure pattern repeats across retries despite different root-cause fixes.
