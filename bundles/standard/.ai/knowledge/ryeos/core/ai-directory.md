<!-- ryeos:signed:2026-07-15T07:49:16Z:2116282a65a162d937312b5c5884d71da1ec6961c885a93ad593710864ffa94e:UMt+VIV4qyOCGpbLTvJpJxZ3CtAN5qguT2CoIiek7Q5aDTVHYtme1ko5pQPR/CRsmxacaPTR9c8Jj+Kiv4zLBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, directory, layout, filesystem]
version: "1.1.0"
description: >
  The exact .ai/ directory layout — bundle structure and the daemon
  state directory, and how they relate.
---

# .ai/ Directory Layout

Rye OS uses `.ai/` directories across two spaces. Each space has a
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
│                                         # python/tool-header, yaml/yaml
├── protocols/ryeos/core/                # cli_exec, opaque, runtime,
│                                         # method_runtime, tool_callback,
│                                         # tool_streaming
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
(default `~/.local/share/ryeos/`, overridable via `RYEOS_APP_ROOT`):

```
<system_space_dir>/
└── .ai/
    ├── config/
    │   └── keys/
    │       ├── signing/private_key.pem  # operator Ed25519 signing key (0600)
    │       └── trusted/<fp>.toml        # trusted publisher/operator/node keys
    ├── node/
    │   ├── config.yaml                  # daemon bind address, db_path, auth config
    │   ├── sandbox.yaml                 # create-once strict execution policy
    │   ├── identity/
    │   │   ├── private_key.pem          # node Ed25519 signing key (0600)
    │   │   └── public-identity.json     # node public identity document
    │   ├── auth/
    │   │   └── authorized_keys/         # <fingerprint>.toml per authorized key
    │   ├── vault/
    │   │   ├── private_key.pem          # X25519 vault encryption key
    │   │   └── public_key.pem
    │   ├── bundles/                     # installed bundle registrations
    │   │   └── <name>.yaml             # path: <abs-path>
    │   ├── verbs/                       # merged from installed bundles
    │   ├── aliases/                     # merged from installed bundles
    │   └── routes/                      # merged from installed bundles
    │
    └── state/
        ├── runtime.sqlite3             # thread/event database (WAL mode)
        ├── objects/                     # CAS object store
        ├── refs/                        # CAS refs
        ├── cache/executions/            # request-owned materialized workspaces
        ├── secrets/
        │   └── store.enc               # encrypted vault (TOML)
        ├── audit/
        │   └── standalone.ndjson       # audit trail
        ├── schedules/
        │   └── <schedule-id>/fires.jsonl
        ├── trace-events.ndjson          # structured trace events
        └── operator.lock                # exclusive daemon lock
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
| `bundles/`       | State only                  | Bundle path registrations    |
| `routes/`        | Bundles + state             | HTTP endpoint definitions    |
| `commands/`      | Bundles + state             | CLI command definitions      |
| `engine/kinds/`  | Loaded by KindRegistry      | Kind schema YAMLs            |
| `identity/`      | Bootstrap-managed           | Node signing keys            |
| `auth/`          | Bootstrap-managed           | Authorized keys              |
| `vault/`         | Bootstrap-managed           | Encryption keys              |

A YAML at `.ai/node/commands/sign.yaml` is a command because of its path.
The loader enforces section containment strictly and rejects duplicated
structural fields such as `section` or `category` in node-config YAML.
