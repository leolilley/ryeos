<!-- ryeos:signed:2026-05-22T19:55:06Z:98ea17bea94cb4b47cf209becb10f86657a47863506ee5b76f513b89b29514fc:6MbDvICd7cK/3mrjE5PQB3rr1n7JrGzJq8TyqDwD4omeFpgcQecWqUlO8fl0x4jTpOH1HD3kzgxeYw08AZsTDA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
