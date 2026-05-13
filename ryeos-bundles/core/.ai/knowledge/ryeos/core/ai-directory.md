---
category: ryeos/core
tags: [reference, directory, layout, filesystem]
version: "1.0.0"
description: >
  The exact .ai/ directory layout — bundle structure, daemon state
  directory, user space overlay, and how they relate.
---

# .ai/ Directory Layout

Rye OS uses `.ai/` directories across three spaces. Each space has a
different layout serving different purposes.

## Bundle Layout (Core)

The core bundle defines the engine's capabilities. Its `.ai/` tree
contains all infrastructure items:

```
.ai/
├── config/
│   └── execution/
│       └── execution.yaml              # subprocess defaults (timeout, steps, cancellation)
│
├── handlers/
│   └── ryeos/core/
│       ├── extends-chain.yaml           # directive inheritance composer
│       ├── graph-permissions.yaml       # graph permission lifting
│       ├── identity.yaml                # no-op pass-through composer
│       ├── regex-kv.yaml                # regex key-value extraction
│       ├── yaml-document.yaml           # full YAML file parser
│       └── yaml-header-document.yaml    # YAML header + body parser (markdown)
│
├── knowledge/
│   └── ryeos/core/                      # 20 knowledge entries (this file is one)
│
├── node/
│   ├── aliases/                         # 21 CLI alias shortcuts
│   │   ├── f.yaml                       # tokens: ["f"] → verb: fetch
│   │   ├── s.yaml                       # tokens: ["s"] → verb: sign
│   │   ├── fetch.yaml                   # tokens: ["fetch"] → verb: fetch
│   │   ├── sign.yaml
│   │   ├── status.yaml
│   │   └── ...                          # (19 more)
│   │
│   ├── engine/
│   │   └── kinds/                       # 12 kind schema definitions
│   │       ├── config/
│   │       │   └── config.kind-schema.yaml
│   │       ├── directive/
│   │       │   └── directive.kind-schema.yaml
│   │       ├── graph/
│   │       │   └── graph.kind-schema.yaml
│   │       ├── handler/
│   │       │   └── handler.kind-schema.yaml
│   │       ├── knowledge/
│   │       │   └── knowledge.kind-schema.yaml
│   │       ├── node/
│   │       │   └── node.kind-schema.yaml
│   │       ├── parser/
│   │       │   └── parser.kind-schema.yaml
│   │       ├── protocol/
│   │       │   └── protocol.kind-schema.yaml
│   │       ├── runtime/
│   │       │   └── runtime.kind-schema.yaml
│   │       ├── service/
│   │       │   └── service.kind-schema.yaml
│   │       ├── streaming_tool/
│   │       │   └── streaming_tool.kind-schema.yaml
│   │       └── tool/
│   │           └── tool.kind-schema.yaml
│   │
│   ├── routes/                          # 7 HTTP route definitions
│   │   ├── execute.yaml                 # POST /execute
│   │   ├── execute-stream.yaml          # POST /execute/stream (SSE)
│   │   ├── health.yaml                  # GET /health (no auth)
│   │   ├── public-key.yaml              # GET /public-key (no auth)
│   │   ├── thread-events-stream.yaml    # GET /threads/{id}/events/stream
│   │   ├── threads-cancel.yaml          # POST /threads/{id}/cancel
│   │   └── threads-detail.yaml          # GET /threads/{id}
│   │
│   └── verbs/                           # 26 CLI verb definitions
│       ├── bundle-install.yaml
│       ├── execute.yaml
│       ├── fetch.yaml
│       ├── sign.yaml
│       ├── status.yaml
│       └── ...                          # (21 more)
│
├── parsers/
│   └── ryeos/core/
│       ├── javascript/
│       │   └── javascript.yaml
│       ├── markdown/
│       │   ├── directive.yaml
│       │   └── frontmatter.yaml
│       ├── python/
│       │   └── ast.yaml
│       └── yaml/
│           └── yaml.yaml
│
├── protocols/
│   └── ryeos/core/
│       ├── opaque.yaml                  # simple tool stdin/stdout
│       ├── runtime_v1.yaml              # full runtime with callbacks
│       └── tool_streaming_v1.yaml       # streaming tool output
│
├── services/
│   ├── bundle/
│   │   ├── install.yaml
│   │   ├── list.yaml
│   │   └── remove.yaml
│   ├── commands/
│   │   └── submit.yaml
│   ├── events/
│   │   ├── chain_replay.yaml
│   │   └── replay.yaml
│   ├── scheduler/
│   │   ├── deregister.yaml
│   │   ├── list.yaml
│   │   ├── pause.yaml
│   │   ├── register.yaml
│   │   ├── resume.yaml
│   │   └── show_fires.yaml
│   ├── threads/
│   │   ├── chain.yaml
│   │   ├── children.yaml
│   │   ├── get.yaml
│   │   └── list.yaml
│   ├── fetch.yaml
│   ├── node-sign.yaml
│   ├── rebuild.yaml
│   └── verify.yaml
│
└── tools/
    └── ryeos/core/
        ├── fetch.yaml
        ├── sign.yaml
        ├── verify.yaml
        ├── identity/
        │   └── public_key.yaml
        ├── parsers/
        │   ├── javascript/javascript.py
        │   ├── markdown/frontmatter.py
        │   ├── markdown/xml.py
        │   ├── python/ast.py
        │   ├── toml/toml.py
        │   └── yaml/yaml.py
        ├── runtimes/
        │   ├── bash/bash.yaml
        │   ├── python/
        │   │   ├── function.yaml
        │   │   ├── script.yaml
        │   │   └── lib/
        │   │       ├── interpolation.py
        │   │       ├── condition_evaluator.py
        │   │       └── module_loader.py
        │   └── state-graph/
        │       ├── runtime.yaml
        │       └── walker.py
        ├── subprocess/
        │   └── execute.yaml
        └── verbs/
            ├── list.py
            └── list.yaml
```

## Bundle Layout (Standard)

The standard bundle adds runtimes, model providers, and agent adapters:

```
.ai/
├── config/
│   ├── keys/
│   │   └── trusted/
│   │       └── <fingerprint>.toml       # publisher Ed25519 public key
│   └── ryeos-runtime/
│       ├── execution.yaml               # API retry/backoff/timeout config
│       ├── model_routing.yaml           # tier → (provider, model) mapping
│       └── model-providers/
│           ├── anthropic.yaml
│           ├── openai.yaml
│           ├── openrouter.yaml
│           └── zen.yaml                 # multi-provider gateway
│
├── runtimes/
│   ├── directive-runtime.yaml           # binary_ref: bin/.../ryeos-directive-runtime
│   ├── graph-runtime.yaml               # binary_ref: bin/.../ryeos-graph-runtime
│   └── knowledge-runtime.yaml           # binary_ref: bin/.../ryeos-knowledge-runtime
│
└── tools/
    └── ryeos/agent/providers/
        ├── anthropic/anthropic.yaml
        ├── openai/openai.yaml
        └── zen/zen.yaml
```

## Daemon State Directory

Created by `ryeos init`. Lives in the system space
(default `~/.local/share/ryeos/`):

```
<system_space_dir>/
└── .ai/
    ├── node/
    │   ├── config.yaml                  # daemon bind address, db_path, auth config
    │   ├── identity/
    │   │   ├── private_key.pem          # node Ed25519 signing key (0600)
    │   │   └── public-identity.json     # node public identity document
    │   ├── auth/
    │   │   └── authorized_keys/         # <fingerprint>.toml per authorized key
    │   ├── vault/
    │   │   ├── private_key.pem          # X25519 vault encryption key
    │   │   └── public_key.pem
    │   ├── bundles/                     # installed bundle registrations
    │   │   └── <name>.yaml             # section: bundles, path: <abs-path>
    │   ├── verbs/                       # merged from installed bundles
    │   ├── aliases/                     # merged from installed bundles
    │   └── routes/                      # merged from installed bundles
    │
    └── state/
        ├── runtime.sqlite3             # thread/event database (WAL mode)
        ├── objects/                     # CAS object store
        ├── refs/                        # CAS refs
        ├── secrets/
        │   └── store.enc               # encrypted vault (TOML)
        ├── audit/
        │   └── standalone.ndjson       # audit trail
        ├── schedules/
        │   └── <schedule-id>/fires.jsonl
        ├── trace-events.ndjson          # structured trace events
        └── operator.lock                # exclusive daemon lock
```

## User Space Overlay

Lives at `~/.ai/`. Used for cross-project personal items:

```
~/.ai/
├── config/
│   └── keys/
│       ├── signing/
│       │   └── private_key.pem         # operator signing key (persistent identity)
│       └── trusted/
│           └── <fingerprint>.toml      # trust documents for verifying items
├── tools/                              # user-level tool overlays
├── knowledge/                          # user-level knowledge overlays
└── directives/                         # user-level directive overlays
```

## Kind-to-Directory Mapping

Each kind schema declares `location.directory` — where items of that
kind live relative to any `.ai/` root:

| Kind            | Directory      | Executable? | Notes                       |
|-----------------|----------------|-------------|-----------------------------|
| `config`        | `config/`      | No          | Per-domain config items     |
| `directive`     | `directives/`  | Yes         | `.md` files only            |
| `graph`         | `graphs/`      | Yes         | `.yaml` files               |
| `handler`       | `handlers/`    | No          | Parser/composer descriptors |
| `knowledge`     | `knowledge/`   | Yes         | `.md` or `.yaml`            |
| `node`          | `node/`        | No          | Sections: verbs, aliases, routes, engine |
| `parser`        | `parsers/`     | No          | Format parser descriptors   |
| `protocol`      | `protocols/`   | No          | Wire protocol descriptors   |
| `runtime`       | `runtimes/`    | Yes         | Runtime binary declarations |
| `service`       | `services/`    | Yes         | In-process service endpoints |
| `streaming_tool`| `tools/`       | Yes         | Same dir as tool, streaming protocol |
| `tool`          | `tools/`       | Yes         | `.py`, `.yaml`, `.js`, `.ts` |

Note: `tool` and `streaming_tool` share the `tools/` directory.
Differentiation is by execution protocol, not directory.

## The `node/` Section Convention

The `node/` directory is special — it contains subdirectories that act
as sections. Each section is scanned separately by the daemon's
bootstrap loader:

| Section          | Who Contributes            | Purpose                      |
|------------------|-----------------------------|------------------------------|
| `aliases/`       | Bundles + state             | CLI token shortcuts          |
| `bundles/`       | State only                  | Bundle path registrations    |
| `routes/`        | Bundles + state             | HTTP endpoint definitions    |
| `verbs/`         | Bundles + state             | CLI verb definitions         |
| `engine/kinds/`  | Loaded by KindRegistry      | Kind schema YAMLs            |
| `identity/`      | Bootstrap-managed           | Node signing keys            |
| `auth/`          | Bootstrap-managed           | Authorized keys              |
| `vault/`         | Bootstrap-managed           | Encryption keys              |

A YAML at `.ai/node/verbs/sign.yaml` must declare `section: verbs`.
The loader enforces this path invariant strictly.
