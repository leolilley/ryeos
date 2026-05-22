<!-- ryeos:signed:2026-05-22T03:35:37Z:40337fe1119dc1fe3b8a6e8509d84a336b8ad25e36b459ca895601b69072e87a:fP54z8aG05yp3rfSsMnO93ipDjG5PzFTmRhZ/jvHmDpJ6/tbFcBBYcNA/YVgMU/O0EqpLsFfQCPWRAHiwbFYDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
permissions:
  execute: []
---

Instructions for the runtime.
```

## Important fields

- `extends`: parent directive ref. Children inherit through `extends-chain`.
- `permissions.execute`: capability strings. Children may narrow but not widen parent effective permissions.
- `context`: knowledge refs grouped by position (`system`, `before`, `after`).
- `model`: optional explicit provider/model/context window; otherwise routing tiers apply.
- `limits`: runtime limits such as turn/token/spend budgets.
- `inputs` / `outputs`: structured contract for callers and summaries.
- `actions`: tool or service actions the runtime may call through callbacks.

Keep directives focused: one job, clear inputs, explicit permissions, and no hidden reliance on project-root provider configs unless trust policy allows it.
