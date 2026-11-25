<!-- ryeos:signed:2026-11-25T06:55:13Z:d24fee4f6521c0b1912f50c5b59b31cb72e74fc344ff011b685fc619d3701e52:AW61HVSzsmufqs3IKXnJRD9apnxwyiysEsg6kZmXHxq9H/Tmv8b1PxL8/KQqmIRKPeGHzpSQpGpzqwKRHh3WDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [fundamentals, kinds, schema, types]
version: "1.1.0"
description: >
  The 12 item kinds in Rye OS — what each kind is for, how it's
  parsed, composed, and executed.
---

# Kind System

Every item in Rye OS has a **kind** — a schema + behavior contract that
determines how the item is parsed, composed, and executed. Kinds are
defined by kind-schema YAML files in the core bundle.

## Kind Contracts

### `directive` — LLM-Facing Workflows
The primary agent-facing item. Directives are markdown files with YAML
frontmatter that define prompts, permissions, limits, context, and
actions for LLM execution.

- **Directory:** `directives/`
- **Formats:** `.md`
- **Parser:** `parser:ryeos/core/markdown/directive`
- **Composer:** `handler:ryeos/core/extends-chain` (full inheritance)
- **Execution:** Delegates to runtime registry (directive-runtime)

Directives support `extends` chains for inheritance, with field-level
merge strategies for body, permissions, and context.

### `tool` — Executable Scripts
The primary executable unit. Tools run as subprocesses with plan-owned opaque
stdin and an explicit signed output protocol. Default wrappers serialize params as
JSON; explicit executor `input_data` may be arbitrary bytes.

- **Directory:** `tools/`
- **Formats:** `.py`, `.yaml`, `.js`, `.ts`, `.json`
- **Composer:** `handler:ryeos/core/identity` (no composition)
- **Execution:** Subprocess via an explicit `execution_protocol` from the Tool allowlist

Tools declare `executor_id: "@subprocess"` which resolves to
`tool:ryeos/core/subprocess/execute`.

For incremental output, the same Tool selects `protocol:ryeos/core/tool_streaming`.
This preserves the admitted executor plan and uses callback-free, non-detachable
framed stdout. See `knowledge:ryeos/core/protocols/tool-streaming`.

### `knowledge` — Context and Documentation
Structured context items injected into LLM prompts. Knowledge can be
markdown (with YAML fenced blocks) or YAML.

- **Directory:** `knowledge/`
- **Formats:** `.md`, `.yaml`
- **Composer:** `handler:ryeos/core/identity`
- **Generic methods:** `compose`, `query`, `graph`, and `validate`
- **Private launch augmentation operation:** `compose_positions`
- **Execution:** method runtime selected by the kind schema and runtime registry

Knowledge methods are directly executable through `call.method`; the
augmentation-private `compose_positions` operation is available only to the
daemon-owned `compose_context_positions` launch step.

### `graph` — State Machines / DAGs
Declarative YAML state machines with nodes, edges, and conditional
branching. Graphs are walked by the state-graph runtime.

- **Directory:** `graphs/`
- **Formats:** `.yaml`
- **Composer:** `handler:ryeos/core/extends-chain` (shallow graph config inheritance)
- **Effective validator:** `handler:ryeos/core/graph-effective-validator`
- **Execution:** Delegates to runtime registry (graph-runtime)

Graphs execute their finalized composed value, not the root YAML in isolation.
Each child redeclares `version` and `category`; omitted `config` keys inherit,
while a declared key such as `nodes` or `hooks` replaces that complete value.
The effective validator rejects incoherent start nodes, edges, expressions,
retry policies, hook policy, and capability facts before launch authority is
minted.

### `config` — Configuration
Per-domain configuration items. Each config consumer enforces its own
schema.

- **Directory:** `config/`
- **Formats:** `.yaml`
- **Execution:** None (not directly executable)

### `handler` — Parser and Composer Descriptors
Registry entries for handlers (parser backends and composition
strategies). Loaded early in bootstrap before kind dispatch.

- **Directory:** `handlers/`
- **Formats:** `.yaml`
- **Execution:** None (engine-internal)

### `parser` — Parser Descriptors
Registry entries for file format parsers. Defines how to extract
metadata from source files.

- **Directory:** `parsers/`
- **Formats:** `.yaml`
- **Execution:** None (engine-internal)

### `protocol` — Wire Protocols
Descriptors for subprocess communication contracts. Defines stdin/stdout
shapes, env injections, capabilities, and lifecycle.

- **Directory:** `protocols/`
- **Formats:** `.yaml`
- **Execution:** None (engine-internal)

### `runtime` — Runtime Binaries
Declared runtime binaries that serve as dispatch targets for directives
and graphs. Each runtime specifies which kind it serves.

- **Directory:** `runtimes/`
- **Formats:** `.yaml`
- **Execution:** Subprocess via `protocol:ryeos/core/runtime`

The runtime item selects a signed implementation binary. When that binary
serves a method-bearing kind, invocation uses the served kind schema's
`execution.method_dispatch.protocol` instead; a method-only runtime is not
directly launchable through the runtime item's ordinary envelope.

### `service` — In-Process Services
Daemon-internal service endpoints registered in the service registry.
Services run in-process (no subprocess overhead).

- **Directory:** `services/`
- **Formats:** `.yaml`
- **Execution:** In-process via service registry

### `node` — Daemon Configuration
Per-node items including verbs, aliases, routes, and engine config.
Items live under `node/<section>/<name>.yaml`.

- **Directory:** `node/`
- **Formats:** `.yaml`
- **Execution:** None (interpreted by node-config loader)

## Kind Schema Fields

Each kind-schema YAML defines:

| Field              | Purpose                                          |
|--------------------|--------------------------------------------------|
| `location.directory` | Which `.ai/` subdirectory items live in         |
| `formats[]`        | Accepted file extensions, parser, and signature  |
| `composer`         | Which handler handles composition                |
| `execution`        | How the kind is executed (subprocess, in-process, delegated) |
| `metadata.rules`   | Which metadata fields to extract                 |

### Effect and workspace declarations: `constant: none`

Under `execution`, `effect_class_ceiling` and `workspace_access` each take
either a `path` (an authored field of the composed item) or a `constant`,
never both and never neither; either mistake, an unknown constant or an
unknown key refuses the kind schema at load. `effect_class_ceiling` accepts the
constants `none`, `recorded`, `sealed` and `live`; `workspace_access` accepts
`none`.

`none` means "no direct tool-call effect or workspace use of its own", and
nothing wider. It does not cover anything the item dispatches: each dispatched
tool is judged against its own declaration at the call. It does not cover a
model call either: a directive's provider calls are judged at launch by the
sealed external-effect authority against any thread ceiling, so a `none`
directive whose launch seals a `sealed` authority is still refused under a
`recorded` ceiling. A declaration is also different from silence: a kind or
item that declares nothing is refused under a thread ceiling and by a detached
launch, while a declared `none` passes. `none` is a preflight judgement only;
capsules and receipts still name only `recorded` or `sealed`, so a durable
request against a `none` subject is ineligible. Only a signed kind schema can
declare `none`; an item that authors `none` at a path declaration is refused.
Kind schemas load only from installed node bundles, each signature-checked against
the trust store, and the first one found for a kind wins; there is no project
overlay and no separate check that the signer is the core publisher. So any
installed bundle from a trusted signer can declare `none` for its own kind: it
is that kind author's claim, and trust in it follows the pinned publisher and
the installed bundle.
