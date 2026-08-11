<!-- ryeos:signed:2026-08-11T02:28:29Z:fcda15599148214b3aa7d9aa41497773e6209a69534255aeed4b4bded82ba1fa:piZz3MfI3JedX4yg/O1RUVNCK5lzJb2PUdJqdOk4MhHZ0AlSDcyazhrw3jasLsVNUaLZDJfWSg9OdiSsOjAmCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# ryeos:signed:2026-06-07T05:37:38Z:a729eb2a41f2fb70cf13c52c86377f9ae66179b4fd3c88bfcad59450ea426794:Xkv/F3+53OFxqXdq1ZndG8cg8q+ZHLx8kI732/mtVtvb/CWKdz9QAWQrp2aot5CT1U1rKFPXHD/PQPvXmLn+CQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
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

1. **Parser output**: the parser handler returns a mapping derived from YAML, markdown frontmatter, Python `# ryeos-tool:` headers, or JavaScript constants.
2. **Composer**: the kind's composer transforms one or more records into an effective record. Core descriptors usually use `identity`; directives and graphs use the generic `extends-chain` composer with signed per-field rules.
3. **Contract check**: the composed value is checked against the kind's required/optional fields.
4. **Policy facts**: composers may derive facts such as `effective_caps`; the runner later mints callback tokens with those caps.
5. **Captured policy**: hook-capable kinds capture authored and signed configured hook layers, their source-owned dispatch grants, and event contracts into the composed view.
6. **Effective validation**: a kind-declared validator checks cross-field semantics over the complete composed value and captured policy. Graph validation proves topology and hook/capability coherence here.
7. **Finalization**: mutable capture proofs are revalidated, then the engine computes one `effective_definition_digest` and seals the immutable program. Only that finalized value can enter a managed launch envelope.
8. **Plan build**: execution metadata becomes a plan: in-process service, subprocess protocol, runtime-registry delegate, or operation dispatch.

## Chain building

Kinds with `resolution` steps can request additional resolution work. Directive
and graph compilation resolve `extends` chains before composition so signed
field strategies can inherit or replace values and narrow capabilities
deterministically. Composition-capable runtimes consume
`resolution.composed.composed`; root bytes remain provenance and are never a
second executable definition.

## Runtime blocks

Tool-like kinds define runtime handlers for `config`, `env_config`, dependency verification, execution params, native async, and resume metadata. Unknown runtime blocks are rejected unless the kind marks the key as metadata/ignored.
