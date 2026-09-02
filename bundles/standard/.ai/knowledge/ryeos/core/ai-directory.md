<!-- ryeos:signed:2026-09-02T21:49:18Z:a7633264ed70a527a434b7399ac99393b337b7aef4659c2688c708c315595f69:MY0N/POn2yxpNPQdnMNQ3rg2WWU6JRXEksQtDMeG5AgYQVnl0/vi0bCE0FNaQDj4BthaPhZqEO0oqfK2evDdCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, directory, layout, filesystem]
version: "1.2.0"
description: >
  The exact .ai/ directory layout — bundle structure and the daemon
  state directory, and how they relate.
---

# .ai/ Directory Layout

Rye OS uses `.ai/` directories across two spaces. Each space has a
different layout serving different purposes.

The project `.ai/` tree is an authored RyeOS control and item surface, not a
general dependency directory. Source that belongs to one executable item stays
beside that item under its kind directory: a namespaced tool may keep helpers
under its tool namespace, and a worker may keep publisher-owned source under
its worker namespace. RyeOS admits that adjacent source as one exact source
closure before execution; it is not declared as external content.

Opaque runtimes, datasets, simulator closures, model files, and other content
trees live outside `.ai/`. Executable items refer to those bytes through
`external_content` with a `project_files` locator and an exact admitted
manifest digest. This keeps kind coverage meaningful, keeps source intuitive
to author, and prevents arbitrary dependency files from being mistaken for
RyeOS items.

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
│   │                                     # streaming_tool, worker
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
│   ├── hooks/base.yaml                  # trusted-bundle hook policy layers
│   ├── model_routing.yaml
│   └── model-providers/
│       ├── anthropic.yaml
│       ├── openai.yaml
│       └── zen.yaml
├── directives/
├── handlers/ryeos/core/
│   ├── extends-chain.yaml
│   └── graph-effective-validator.yaml
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
    │   ├── config.yaml                  # daemon bootstrap paths and listener addresses
    │   ├── isolation.yaml                 # create-once strict execution policy
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
| `worker`        | `workers/`     | Yes         | Persistent subprocess definition + adjacent source |

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
