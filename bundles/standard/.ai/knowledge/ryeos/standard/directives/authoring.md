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
