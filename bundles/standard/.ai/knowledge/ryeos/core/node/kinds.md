<!-- ryeos:signed:2026-07-15T07:49:18Z:33e9fdbb1c6d8563bd1968498c69ebf53d315849be3ca3a05e3e11d66ee3a908:flyhSHoJCzt4i/lZoAUdu0TjLv0i3JpSbq+Pr1GCf5fxJ+auM1hU3XOx7U+kDhPFgJantN5TPqMMg9ZSFJpZAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [fundamentals, kinds, schema, types]
version: "1.0.0"
description: >
  The 12 item kinds in Rye OS — what each kind is for, how it's
  parsed, composed, and executed.
---

# Kind System

Every item in Rye OS has a **kind** — a schema + behavior contract that
determines how the item is parsed, composed, and executed. Kinds are
defined by kind-schema YAML files in the core bundle.

## The 12 Kinds

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
stdin and return opaque bytes on stdout. Default wrappers serialize params as
JSON; explicit executor `input_data` may be arbitrary bytes.

- **Directory:** `tools/`
- **Formats:** `.py`, `.yaml`, `.js`, `.ts`, `.json`
- **Composer:** `handler:ryeos/core/identity` (no composition)
- **Execution:** Subprocess via `protocol:ryeos/core/tool_callback`

Tools declare `executor_id: "@subprocess"` which resolves to
`tool:ryeos/core/subprocess/execute`.

### `streaming_tool` — Streaming Executables
Same as `tool` but emits length-prefixed JSON frames on stdout for
streaming output during execution.

- **Execution:** Subprocess via `protocol:ryeos/core/tool_streaming`

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
- **Composer:** `handler:ryeos/core/graph-permissions`
- **Execution:** Delegates to runtime registry (graph-runtime)

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
