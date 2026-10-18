<!-- ryeos:signed:2026-09-10T02:15:35Z:e1452dc0eb79f8bb59c84b02236774f69d76e2884b89a8628e0c59b842dc53e2:hxARYKWB4WfA07fYHDeyglbUGpS7GrIfd+5FXlgTyRCe9wieSPrdm7T+hG/zPOKOcybukEq004snR7NKHPATDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [fundamentals, architecture, overview]
version: "1.2.0"
description: >
  The Rye OS mental model — how items, kinds, bundles, spaces, and the
  daemon fit together. Read this first to understand the system.
---

# Rye OS Mental Model

RyeOS is a **content-addressed, signed, capability-gated execution system**
for ordinary and model-driven work. Authored behaviour and context are expressed
as typed *items*. Executions retain their own admitted authority and history;
they are distinct from the processes and models performing them.

## Start with work, not a cast of agents

A directive authors executable intent for a model. It is not necessarily an
assistant persona, a persistent entity or a signing principal. The same
definition can have many separately admitted executions.

| Concept | Responsibility |
|---|---|
| Directive | Authored model-driven work, with composed context, requirements and limits |
| Execution | One admitted invocation and its retained history |
| Principal and authority | Who requests, authorises or stands behind an act |
| Model/runtime | Performs the model-driven portion of execution |
| Project/campaign | Organises objectives, methods, evidence and acceptance |

Tools perform concrete operations; directives can make bounded judgments or
run tool loops; graphs coordinate explicit transitions. Required evaluation
and publication boundaries should not depend solely on a model declaring
success. This division still leaves room for open-ended exploration within
the admitted scope.

An assistant can be built from these mechanisms. External clients may call
their own abstractions agents; RyeOS does not require that framing to author
work. See [Why RyeOS](why-ryeos.md), [Identity model](identity-model.md) and
[Directives](../standard/directives/directives.md).

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
Authored items have a *kind* (directive, tool,
knowledge, config, etc.) that determines how they are parsed, composed,
and executed. Items live in directories determined by their kind schema
(`location.directory`). The actual layout varies by space (bundle vs
daemon state). See `knowledge:ryeos/core/ai-directory`
for the full tree. The conceptual directory mapping:

| Directory      | Kind(s)          | What Lives Here                     |
|----------------|------------------|-------------------------------------|
| `directives/`  | directive        | `.md` prompt workflows              |
| `tools/`       | tool             | `.py`, `.yaml`, `.js` executables |
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

The installed signed bundle set determines available kinds. These include
`directive`, `tool`, `knowledge`, `graph`, `config`, `handler`, `parser`,
`protocol`, `runtime`, `service`, `node`, `worker` and `worker_execution`.

### Canonical Refs
Items are addressed by **canonical ref**: `kind:path/to/item`

Examples:
- `directive:my-project/deploy` → `.ai/directives/my-project/deploy.md`
- `tool:ryeos/core/sign` → `.ai/tools/ryeos/core/sign.yaml`
- `knowledge:ryeos/core/mental-model` → `.ai/knowledge/ryeos/core/mental-model.md`

The kind prefix determines which directory to look in. The path is
slash-separated, without file extension.

### Two-Tier Space Resolution
Items resolve **project → system** (first match wins):

| Space   | Location                 | Purpose                      |
|---------|--------------------------|------------------------------|
| Project | `.ai/`                   | Project-specific items       |
| System  | Bundle `.ai/` directories | Immutable standard library   |

When you `fetch` or `execute` an item, the engine checks project first,
then all installed bundles.

### Bundles
A **bundle** is a signed, self-contained `.ai/` tree. Two bundles ship
with the system:
- **core** — engine/control-plane kinds, parsers, handlers, protocols,
  tools, verbs, routes, and services
- **standard** — workflow kinds, workflow handlers/parsers, runtimes,
  model providers/routing, and thread/scheduler/events services

Additional bundles can be installed via `ryeos bundle install`.

Current split:

- **Core** owns the engine/control-plane layer: core kinds, parsers,
  protocols, services, tools, route/verb/alias descriptors, remote/vault/CAS
  services, and node bootstrap metadata.
- **Standard** owns the workflow layer: directive/graph/knowledge kinds,
  workflow composers and parser, directive/graph/knowledge runtimes, model
  routing/provider configs, and thread/scheduler/events/commands services.

### Signing
Every item carries an Ed25519 signature in a header comment. The signature
covers the content hash, anchoring the item to its file path. Signing
establishes **trust** — unsigned or tampered items are rejected at execution
time (with a clear error telling you exactly what to fix).

Use `ryeos sign <ref>` to sign items. Use `ryeos verify <ref>` to check them.

### Capabilities
Execution is **capability-gated**. Items declare which capabilities they need
under `requires.capabilities.declared` (a flat list of self-asserted caps). The
daemon checks these against the calling context before allowing execution.
Capabilities use dot-namespaced strings like `ryeos.execute.tool.ryeos.file-system.*`.

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
