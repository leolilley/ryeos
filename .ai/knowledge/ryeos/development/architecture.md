---
category: "ryeos/development"
name: "architecture"
description: "Rust crate architecture, workspace structure, and how components connect"
---

# Architecture

## Workspace crates

```
ryeos-cas-as-truth/
├── lillux/              # Crypto primitives (Ed25519, X25519, SHA-256)
├── ryeos-engine/        # Core engine: resolution, verification, plan building
├── ryeos-state/         # SQLite-backed state store, CAS objects, thread state
├── ryeos-runtime/       # Shared runtime library (callback client, envelope)
├── ryeos-handler-protocol/  # Handler subprocess protocol types
├── ryeos-handler-bins/  # Parser/composer binary implementations
├── ryeos-tracing/       # Structured tracing utilities
├── ryeos-tools/         # CLI action implementations (init, publish, trust, vault)
├── ryeosd/              # The daemon (HTTP + UDS server)
├── ryeos-cli/           # The CLI binary (`ryeos`)
├── ryeos-directive-runtime/  # Directive execution subprocess
├── ryeos-graph-runtime/      # State graph execution subprocess
├── ryeos-knowledge-runtime/  # Knowledge composition subprocess
├── ryeos-bundles/       # Bundle source trees (core + standard)
│   ├── core/            # Kind schemas, parsers, handlers, protocols, tools
│   └── standard/        # Runtimes, model providers, directives
├── ryeosd-mcp/          # Python MCP adapter (wraps CLI binary)
├── scripts/             # Build/gate/dev scripts
└── .dev-keys/           # Development publisher keypair
```

## Dependency flow

```
lillux (crypto primitives)
  └── ryeos-engine (resolution, verification, plan building)
        ├── ryeos-state (SQLite CAS, thread state)
        ├── ryeos-runtime (callback client, shared runtime types)
        └── ryeos-handler-protocol (subprocess protocol)

ryeos-tools (CLI actions: init, publish, trust, vault)
  └── ryeos-engine, lillux

ryeosd (the daemon)
  └── ryeos-engine, ryeos-state, ryeos-runtime, ryeos-tracing

ryeos-cli (the CLI binary)
  └── ryeos-tools, ryeos-engine

ryeos-directive-runtime (subprocess runtime)
  └── ryeos-runtime, ryeos-engine

ryeos-graph-runtime (subprocess runtime)
  └── ryeos-runtime

ryeos-knowledge-runtime (subprocess runtime)
  └── ryeos-runtime

ryeos-handler-bins (parser/composer binaries)
  └── ryeos-handler-protocol
```

## Execution flow

```
CLI: ryeos execute tool:ryeos/core/identity/public_key
  │
  ▼  CLI resolves daemon URL from daemon.json
  │    Signs request with node identity key
  │
  ▼  POST /execute to ryeosd (HTTP or UDS)
  │
Daemon: receives request
  │
  ▼  Engine resolves item through spaces (project → user → system)
  │    - Looks up kind schema for "tool"
  │    - Finds matching file via parser descriptors
  │    - Verifies signature against trust store
  │
  ▼  Plan builder creates execution plan
  │    - Determines handler (e.g., Subprocess)
  │    - Selects runtime from registry
  │    - Builds protocol envelope (env vars, callback tokens)
  │
  ▼  Dispatch subprocess
  │    - Spawns runtime binary (e.g., ryeos-core-tools)
  │    - Injects env vars via protocol descriptor
  │    - Runtime calls back to daemon for sub-actions
  │
  ▼  Collect result, write state transition to CAS chain
  │
  ▼  Return result to CLI
```

## The bundle system

Bundles are content-addressed directory trees. Two bundles ship with the system:

### Core bundle (`ryeos-bundles/core/`)

Infrastructure that the daemon needs to function:

| Directory | Contents |
|---|---|
| `.ai/node/engine/kinds/` | Kind schemas (directive, tool, knowledge, graph, service, runtime, etc.) |
| `.ai/node/engine/` | Parser tool descriptors, handler descriptors, protocol descriptors |
| `.ai/parsers/` | Parser implementations (YAML, markdown, Python AST, JavaScript) |
| `.ai/handlers/` | Handler implementations (subprocess, identity, extends-chain) |
| `.ai/protocols/` | Protocol descriptors (runtime_v1, tool_streaming_v1, opaque) |
| `.ai/services/` | Operational services (fetch, sign, verify, identity, rebuild, events) |
| `.ai/tools/` | Core tools (sign, verify, fetch, identity, subprocess, verbs, runtimes) |
| `.ai/config/` | Execution config |
| `.ai/node/routes/` | HTTP route definitions |
| `.ai/node/verbs/` | CLI verb definitions |
| `.ai/node/aliases/` | CLI alias shortcuts |
| `.ai/bin/<triple>/` | Compiled binaries (ryeos-core-tools, parsers, composers) |

### Standard bundle (`ryeos-bundles/standard/`)

User-facing runtimes and configuration:

| Directory | Contents |
|---|---|
| `.ai/runtimes/` | Runtime definitions (directive, graph, knowledge) |
| `.ai/config/ryeos-runtime/` | Model providers, model routing, execution config |
| `.ai/tools/ryeos/agent/providers/` | LLM provider adapter tools (Anthropic, OpenAI, Zen) |
| `.ai/directives/` | Example directives (hello.md) |
| `.ai/bin/<triple>/` | Runtime binaries (directive-runtime, graph-runtime, knowledge-runtime) |

## Key subsystems

### Kind schemas

Define item types. Each kind declares:
- File formats and parsers
- Execution model (operations, dispatch method)
- Metadata rules (how name/category are derived from path)
- Composer (how items are composed/merged)

Located at `<root>/.ai/node/engine/kinds/<name>/<name>.kind-schema.yaml`.

### Handlers

Define how to execute items. The primary handler is `Subprocess` which spawns a binary with a protocol envelope. Other handlers include `Identity` (pass-through) and composition handlers.

### Protocols

Define the wire contract between daemon and subprocess. Specify which env vars to inject, how stdin/stdout are framed, what capabilities are available.

### Verb/alias system

The CLI dispatches through a data-driven verb table:
- **Verbs** (`node/verbs/*.yaml`): Full verb definitions with parameter binding
- **Aliases** (`node/aliases/*.yaml`): Shortcuts that map to verb calls

The CLI tries local verbs first (init, publish, trust, vault), then `execute <canonical-ref>`, then token-based dispatch through the verb table.

### Trust store

Three-tier trust model:
- **Node key**: Daemon's Ed25519 identity, generated at init
- **User key**: Operator's Ed25519 identity, generated at init
- **Trusted signers**: Pinned public keys in `~/.ai/config/keys/trusted/*.toml`

At boot, the daemon loads the trust store and verifies every bundle item against it. Untrusted items are rejected.
