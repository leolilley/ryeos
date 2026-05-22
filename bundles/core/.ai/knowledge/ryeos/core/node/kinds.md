<!-- ryeos:signed:2026-05-22T03:35:36Z:0f4118c187fcb6d75a557c79ecd585162f0857da1f65eece8cd81e8308130ad4:pSKHCHj6gHbe5zGgLBPO9Xzd+/fNq7zsQIZdC5x17lSGJamM0bLm13CSxe8PV2CFFgoeJjiF/26JvRcs7S1LCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
The primary executable unit. Tools run as subprocesses that receive
JSON params on stdin and return opaque bytes on stdout.

- **Directory:** `tools/`
- **Formats:** `.py`, `.yaml`, `.js`, `.ts`, `.json`
- **Composer:** `handler:ryeos/core/identity` (no composition)
- **Execution:** Subprocess via `protocol:ryeos/core/opaque`

Tools declare `executor_id: "@subprocess"` which resolves to
`tool:ryeos/core/subprocess/execute`.

### `streaming_tool` — Streaming Executables
Same as `tool` but emits length-prefixed JSON frames on stdout for
streaming output during execution.

- **Execution:** Subprocess via `protocol:ryeos/core/tool_streaming_v1`

### `knowledge` — Context and Documentation
Structured context items injected into LLM prompts. Knowledge can be
markdown (with YAML fenced blocks) or YAML.

- **Directory:** `knowledge/`
- **Formats:** `.md`, `.yaml`
- **Composer:** `handler:ryeos/core/identity`
- **Execution:** Compose operations (`compose`, `compose_positions`)

Knowledge is not directly executable — it's assembled into prompt
context blocks by the compose operation.

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
- **Execution:** Subprocess via `protocol:ryeos/core/runtime_v1`

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
