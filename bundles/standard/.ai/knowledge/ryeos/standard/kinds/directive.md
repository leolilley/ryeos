---
category: ryeos/standard/kinds
tags: [kind, directive, llm, workflow]
version: "1.0.0"
description: Directive kind reference.
---

# Kind: directive

Invariant: directives are markdown LLM workflows whose effective body, permissions, and context are composed before the directive runtime launches.

- Directory: `directives/`
- Format: `.md` via `parser:ryeos/core/markdown/directive`
- Composer: `handler:ryeos/core/extends-chain`
- Execution: delegates through runtime registry to `runtime:directive-runtime`
- Policy facts: `permissions.execute` becomes `effective_caps`
- Launch augmentation: composed context positions are rendered through the knowledge runtime before launch

Directive inheritance keeps the root body verbatim, narrows child permissions against parent effective permissions, and merges context blocks root-last by position.
