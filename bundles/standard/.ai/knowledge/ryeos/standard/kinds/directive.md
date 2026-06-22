<!-- ryeos:signed:2026-06-22T04:23:11Z:8316696c77836a446ace577106f7e4cd6f8156772d00cc9c43d1bbd9e57f5387:DoPSij+zynb1byVV2WSNHIiCIfbSszCh9vWZ6FuYg2I2bmMiUiIjPfaRKliYFxH/zHCVz138HqWoe9M4VaPlBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Directive inheritance keeps the root body verbatim, narrows child permissions against parent effective permissions, and merges context blocks root-last by position.
