---
category: ryeos/core/engine
tags: [engine, compilation, compose, handlers, plan-builder]
version: "1.0.0"
description: >
  The composition and plan-building phase after an item is resolved.
---

# Engine Compilation

Invariant: compilation validates and normalizes a resolved item before execution so launchers consume a uniform plan, not raw source files.

## Compile stages

1. **Parser output**: the parser handler returns a mapping derived from YAML, markdown frontmatter, Python dunders, or JavaScript constants.
2. **Composer**: the kind's composer transforms one or more records into an effective record. Core descriptors usually use `identity`; directives use `extends-chain`; graphs use `graph-permissions`.
3. **Contract check**: the composed value is checked against the kind's required/optional fields.
4. **Policy facts**: composers may derive facts such as `effective_caps`; the runner later mints callback tokens with those caps.
5. **Plan build**: execution metadata becomes a plan: in-process service, subprocess protocol, runtime-registry delegate, or operation dispatch.

## Chain building

Kinds with `resolution` steps can request additional resolution work. Directive compilation resolves `extends` chains before composition so field merge strategies can narrow permissions and merge context blocks deterministically.

## Runtime blocks

Tool-like kinds define runtime handlers for `config`, `env_config`, dependency verification, execution params, native async, and resume metadata. Unknown runtime blocks are rejected unless the kind marks the key as metadata/ignored.
