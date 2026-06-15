<!-- ryeos:signed:2026-06-11T21:03:05Z:98ea17bea94cb4b47cf209becb10f86657a47863506ee5b76f513b89b29514fc:SNF+mE3j8ibko8pPg4nq2Y11rhTHRhRRr8eA93Av02ZXs2XuryUNVGtO9sHu0uaHsOpnN18SjJ6mEJLx2KofDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
