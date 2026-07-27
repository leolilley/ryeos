<!-- ryeos:signed:2026-07-27T04:06:35Z:63d146c3be0e544edbe4b10153fd9df52b2aeb3dfbbbbb8f6573cdd5fba033b3:+oyI8RH/E+CnHW/MUnYlyEfAIlGNk9+dz9Su//iWHqqY9/PEckX2HBb/AuYEuZplZdz6ASPGf+07oJ5yAPt/AA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/directives
tags: [directive, authoring, frontmatter]
version: "1.0.0"
description: How to author directive markdown files.
---

# Directive Authoring

Invariant: a directive file is signed markdown whose YAML metadata is composed and whose body is the prompt executed by directive-runtime.

## Minimal shape

```markdown
---
category: my/project
description: Do one specific job.
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.echo
---

Instructions for the runtime.
```

## Important fields

- `extends`: parent directive ref. Children inherit through `extends-chain`.
- `requires.capabilities.declared`: a flat list of self-asserted capability strings (the cap encodes its own verb, e.g. `ryeos.execute.tool.echo`). Children may narrow but not widen the parent's declared set.
- `requires.capabilities.manifest.runtime_authority`: runtime callback authority (bundle events / vault / item authoring) the daemon mints only as the signed bundle manifest backs it — not self-grantable.
- `context`: knowledge refs grouped by position (`system`, `before`, `after`).
- `model`: optional explicit provider/model/context window; otherwise routing tiers apply.
- `limits`: runtime limits such as turn/token/spend budgets.
- `inputs` / `outputs`: structured contract for callers and summaries.
- `actions`: tool or service actions the runtime may call through callbacks.
- `execution.tool_concurrency` (default 4, range 1..=16): how many tool calls
  from ONE assistant message dispatch concurrently. Results always fold back
  in call order — the provider transcript is identical to a serial run — while
  the braid records the real shape (a batch's `tool_call_start` intents first,
  then results; consumers pair by `call_id`, never adjacency). `1` is strict
  serial dispatch. Batches carrying `directive_return` always run serially.
  Each in-flight dispatch holds one dedicated daemon connection for the
  child's whole duration, so raise the bound with the node's connection
  budget and concurrent-directive count in mind.

Keep directives focused: one job, clear inputs, explicit declared capabilities, and no hidden reliance on project-root provider configs unless trust policy allows it.

> Not to be confused with **runtime item authoring** — how an executing runtime
> proposes a new signed project item through the daemon `runtime.author_item`
> callback. That is a separate capability; see `ryeos/standard/item-authoring`.
