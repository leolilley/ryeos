---
category: ryeos/core
tags: [fundamentals, architecture, overview]
version: "1.0.0"
description: >
  The Rye OS mental model — how items, kinds, bundles, spaces, and the
  daemon fit together. Read this first to understand the system.
---

# Rye OS Mental Model

Rye OS is a **content-addressed, signed, capability-gated execution engine**
for AI agents. Everything in the system is an *item* — a file in `.ai/`
that carries typed metadata and optional body content.

## The Big Picture

```
┌─────────────────────────────────────────────────┐
│                   CLI / MCP                      │
│            (ryeos fetch/execute/sign)            │
├─────────────────────────────────────────────────┤
│                  Daemon                          │
│  ┌──────────┐ ┌───────────┐ ┌────────────────┐  │
│  │ Routes   │ │ Services  │ │ Thread Manager │  │
│  │ (HTTP)   │ │ (in-proc) │ │                │  │
│  └──────────┘ └───────────┘ └────────────────┘  │
│  ┌──────────────────────────────────────────┐    │
│  │          Engine (CAS + Kinds)             │    │
│  │  parse → compose → sign → verify         │    │
│  └──────────────────────────────────────────┘    │
├─────────────────────────────────────────────────┤
│              Runtimes (subprocess)               │
│  directive-runtime │ graph-runtime │ tools       │
└─────────────────────────────────────────────────┘
```

## Core Concepts

### Items
Every `.ai/` file is an **item**. Items have a *kind* (directive, tool,
knowledge, config, etc.) that determines how they are parsed, composed,
and executed. Items live in directories determined by their kind schema
(`location.directory`). The actual layout varies by space (bundle vs
daemon state vs user overlay). See `knowledge:ryeos/core/ai-directory`
for the full tree. The conceptual directory mapping:

| Directory      | Kind(s)          | What Lives Here                     |
|----------------|------------------|-------------------------------------|
| `directives/`  | directive        | `.md` prompt workflows              |
| `tools/`       | tool, streaming_tool | `.py`, `.yaml`, `.js` executables |
| `knowledge/`   | knowledge        | `.md`, `.yaml` context entries      |
| `config/`      | config           | `.yaml` configuration items         |
| `graphs/`      | graph            | `.yaml` state machines / DAGs       |
| `handlers/`    | handler          | Parser and composer descriptors     |
| `parsers/`     | parser           | Format parser descriptors           |
| `protocols/`   | protocol         | Wire protocol descriptors           |
| `runtimes/`    | runtime          | Runtime binary declarations         |
| `services/`    | service          | In-process service endpoints        |
| `node/`        | node             | Verbs, aliases, routes, engine/kinds|

### Kinds
A **kind** is a schema + behavior contract. Each kind defines:
- What directory its items live in
- What file formats and parsers it accepts
- What composer handles inheritance/merging
- How execution works (subprocess, in-process, delegated)

There are 12 built-in kinds: `directive`, `tool`, `streaming_tool`, `knowledge`,
`graph`, `config`, `handler`, `parser`, `protocol`, `runtime`, `service`, `node`.

### Canonical Refs
Items are addressed by **canonical ref**: `kind:path/to/item`

Examples:
- `directive:my-project/deploy` → `.ai/directives/my-project/deploy.md`
- `tool:ryeos/core/sign` → `.ai/tools/ryeos/core/sign.yaml`
- `knowledge:ryeos/core/mental-model` → `.ai/knowledge/ryeos/core/mental-model.md`

The kind prefix determines which directory to look in. The path is
slash-separated, without file extension.

### Three-Tier Space Resolution
Items resolve **project → user → system** (first match wins):

| Space   | Location                 | Purpose                      |
|---------|--------------------------|------------------------------|
| Project | `.ai/`                   | Project-specific items       |
| User    | `~/.ai/`                 | Cross-project personal items |
| System  | Bundle `.ai/` directories | Immutable standard library   |

When you `fetch` or `execute` an item, the engine checks project first,
then user, then all installed bundles.

### Bundles
A **bundle** is a signed, self-contained `.ai/` tree. Two bundles ship
with the system:
- **core** — kinds, parsers, handlers, protocols, tools, verbs, services
- **standard** — runtimes, model providers, routing, trusted keys

Additional bundles can be installed via `ryeos bundle install`.

### Signing
Every item carries an Ed25519 signature in a header comment. The signature
covers the content hash, anchoring the item to its file path. Signing
establishes **trust** — unsigned or tampered items are rejected at execution
time (with a clear error telling you exactly what to fix).

Use `ryeos sign <ref>` to sign items. Use `ryeos verify <ref>` to check them.

### Capabilities (Permissions)
Execution is **capability-gated**. Directives and tools declare which
capabilities they need in `permissions.execute`. The daemon checks these
against the calling context before allowing execution. Capabilities use
dot-namespaced strings like `ryeos.execute.tool.ryeos.file-system.*`.

### Threads
Every execution runs in a **thread** — a tracked unit of work with its
own ID, events log, and lifecycle. Threads can be listed, tailed,
cancelled, and replayed. Thread trees allow parent-child relationships
(e.g., a directive spawning sub-tasks).

### The Daemon
The daemon (`ryeosd`) is a long-running process that holds the CAS
(content-addressed store), manages threads, serves HTTP routes, and
dispatches execution. The CLI talks to the daemon over HTTP.

## Data Flow

```
CLI command → verb → service → engine → kind → parser → composer → executor
                                                              ↓
                                                         subprocess
```

1. **CLI** parses the verb and arguments
2. **Verb** routes to a service or tool
3. **Engine** resolves the item by canonical ref across spaces
4. **Kind** determines which parser and composer to use
5. **Parser** extracts metadata from the file
6. **Composer** handles inheritance (extends chains for directives)
7. **Executor** runs the item (subprocess for tools/runtimes, in-process for services)
