<!-- ryeos:signed:2026-05-31T08:15:56Z:ed6e6f4881f22a07181cf4d1719d9b668134aed7137ad970969fa613897dd602:k3V+s6TpRljuKsCJtX3RwZobuvfgT/Ld4f2TJwsN7+ff4RLHY0jwDKp+afrp0E43XM9+VRAFiqIIwtqdBarnBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

The core bundle is the engine/control-plane layer. It contains the generic
machine, not the LLM workflow layer:

```
.ai/
├── config/execution/execution.yaml
├── handlers/ryeos/core/
│   ├── identity.yaml
│   ├── regex-kv.yaml
│   ├── yaml-document.yaml
│   └── yaml-header-document.yaml
├── knowledge/ryeos/core/
├── node/
│   ├── aliases/                         # core CLI aliases + remote/vault aliases
│   ├── engine/kinds/                    # config, handler, parser, protocol,
│   │                                     # runtime, service, node, tool,
│   │                                     # streaming_tool
│   ├── routes/                          # execute, health, public-key,
│   │                                     # objects, vault, remote status, push-head
│   └── verbs/                           # core, bundle, remote, vault, maintenance verbs
├── parsers/ryeos/core/                  # javascript, markdown/frontmatter,
│                                         # python/ast, yaml/yaml
├── protocols/ryeos/core/                # opaque, runtime_v1, tool_streaming_v1
├── services/                            # bundle, fetch, verify, objects,
│                                         # remote, vault, system, health, etc.
└── tools/ryeos/core/                    # fetch/sign/verify, identity,
                                          # subprocess, python runtimes, verbs/list
```

The active core bundle layout is the source of truth for parser,
handler, service, protocol, tool, route, verb, and alias descriptors.

## Bundle Layout (Standard)

The standard bundle is the agent workflow layer. It contributes workflow
kinds, composers, runtime binaries, model routing, and workflow services:

```
.ai/
├── config/ryeos-runtime/
│   ├── execution.yaml
│   ├── model_routing.yaml
│   └── model-providers/
│       ├── anthropic.yaml
│       ├── openai.yaml
│       └── zen.yaml
├── directives/
├── handlers/ryeos/core/
│   ├── extends-chain.yaml
│   └── graph-permissions.yaml
├── knowledge/ryeos/standard/
├── node/
│   ├── aliases/                         # thread/events/commands/compose aliases
│   ├── engine/kinds/                    # directive, graph, knowledge
│   ├── routes/                          # thread event stream + cancel
│   └── verbs/                           # thread, scheduler, events, commands, compose
├── parsers/ryeos/core/markdown/directive.yaml
├── runtimes/
│   ├── directive-runtime.yaml
│   ├── graph-runtime.yaml
│   └── knowledge-runtime.yaml
└── services/                            # threads, scheduler, events, commands
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

Lives at `~/.ryeos/.ai/`. Used for cross-project personal items:

```
~/.ryeos/.ai/
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
