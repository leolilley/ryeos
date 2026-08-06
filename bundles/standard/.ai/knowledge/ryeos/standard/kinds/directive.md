<!-- ryeos:signed:2026-08-05T07:04:40Z:c8b4fa41312956d9d16446fd0e4509663f1cab32717fd729c4f12a12bdba3a7f:cBAKjOJrXE2HQykaWOpxHtwjBJe4OdlzRe5T4BLKAwCbOcFUIIi6fvPwU4Kqxatl2j/ex1qVAdzA8+oY0j1mCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
- Policy facts: `requires.capabilities.declared` becomes `effective_caps`
- Launch augmentation: composed context positions are rendered through the knowledge runtime before launch
- Hooks: authored `hooks` inherit or replace atomically; configured layers are captured before launch

Directive inheritance keeps the root body verbatim, narrows child permissions
against parent effective permissions, merges context blocks root-last by
position, and treats hook policy as one nearest complete list. After declared
augmentation, the daemon captures the effective hook plan, validates and seals
the full resolution, and the runtime recomputes its
`effective_definition_digest` before execution.
