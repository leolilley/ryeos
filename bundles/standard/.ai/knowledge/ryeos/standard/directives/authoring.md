<!-- ryeos:signed:2026-07-03T01:05:59Z:9dd011676146de18218b569acab852127ccc2ce84e0513af593c0bcb35f964c3:bZCXC1Rq4yyS6pCq9VbjLnGvGK4KktyuE3O5JHCxMsKF6puUtBERPei2sjD4lQiiYRLeJID4HPZOuZfY+CyxBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Keep directives focused: one job, clear inputs, explicit declared capabilities, and no hidden reliance on project-root provider configs unless trust policy allows it.

> Not to be confused with **runtime item authoring** — how an executing runtime
> proposes a new signed project item through the daemon `runtime.author_item`
> callback. That is a separate capability; see `ryeos/standard/item-authoring`.
