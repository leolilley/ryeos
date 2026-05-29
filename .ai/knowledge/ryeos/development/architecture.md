<!-- ryeos:signed:2026-05-25T06:47:53Z:38ce7f7cbeaaf795ac1074b9d68520c7f5de467cce0ee505dfe20d5bf1698a5f:KR2uDoR6ECGP4AnthtdE6qvTKlBQf8ReGWeombYFZKcdTQN/WyZpd8dRgqDFepf8oAOss1jlBaE3RXHahx6IAw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
```yaml
category: "ryeos/development"
name: "architecture"
title: "Architecture Map"
description: "Short orientation for crates, bundles, execution flow, and trust boundaries"
entry_type: reference
version: "1.2.0"
```

# Architecture Map

Use this for orientation before changing code. It is intentionally a map, not a
full design document.

## Main crate map

| Area | Path | Owns |
|---|---|---|
| Crypto | `crates/kernel/lillux/` | Ed25519/X25519/SHA primitives and signatures |
| Engine | `crates/engine/ryeos-engine/` | item resolution, trust verification, composition, plans |
| App | `crates/daemon/ryeos-app/` | daemon/app config, engine boot, node-config loading |
| State | `crates/state/ryeos-state/` | SQLite state, CAS objects, thread state |
| Runtime shared | `crates/engine/ryeos-runtime/` | callback client, runtime envelopes/types |
| Tools | `crates/tools/core-tools/` | init, bundle build/verify, trust, vault, sign/fetch actions |
| CLI | `crates/bin/cli/` | `ryeos` command dispatch and daemon transport |
| Daemon | `crates/bin/daemon/` | HTTP/UDS server and execution API |
| Handler bins | `crates/tools/handler-bins/` | parser/composer subprocesses |
| Runtimes | `crates/runtimes/{directive,graph,knowledge}/` | bundled execution runtimes |
| TUI model | `crates/clients/base/` | platform-agnostic UI state/update/views |
| TUI terminal | `crates/clients/terminal/` | `ryeos-tui` binary |
| MCP adapter | `integrations/mcp/ryeosd/` | Python MCP wrapper around `ryeos` |

## Bundle map

Bundles are signed content trees. Derived bundle state (`.ai/bin`, `.ai/objects`,
`.ai/refs`) is rebuilt by `scripts/populate-bundles.sh`.

| Bundle | Path | Contains |
|---|---|---|
| Core | `bundles/core/` | kind schemas, parsers, handlers, protocols, services, core tools, routes, CLI aliases/verbs |
| Standard | `bundles/standard/` | directive/graph/knowledge runtimes, model provider config, user-facing clients/tools/directives |

Important bundle subdirs:

| Subdir | Meaning |
|---|---|
| `.ai/node/engine/kinds/` | kind schemas |
| `.ai/node/verbs/` | CLI verb descriptors |
| `.ai/node/aliases/` | CLI alias descriptors |
| `.ai/services/`, `.ai/tools/`, `.ai/clients/` | executable/composable item descriptors |
| `.ai/bin/<triple>/` | trusted bundle-owned binaries |

## Execution flow

```text
ryeos CLI
  -> signs request to ryeosd
  -> daemon boots engine from registered bundles
  -> engine resolves project -> user -> system
  -> engine verifies trust and composes effective item
  -> plan/handler/protocol selects runtime or tool binary
  -> subprocess runs and may call back to daemon
  -> daemon records state and returns result
```

Local/offline CLI dispatch follows the same principle: load installed bundle
roots, call `engine.effective_item(... expected_kind: None ...)`, then inspect
generic composed dispatch fields. Avoid kind-specific CLI descriptor parsing.

## Trust and signing boundaries

- Bundle items are publisher-signed and verified against trusted publisher keys.
- Project/user items are operator-signed with the user key.
- Installed bundles are discovered from signed node bundle registrations, not
  arbitrary ambient directories.
- Bundle-owned binaries must be resolved from signed bundle bin trees. Do not
  install handler/runtime/tool binaries on PATH as a workaround.

## Where to change things

| Need | Likely area |
|---|---|
| Item resolution/composition behavior | `crates/engine/ryeos-engine/src/` |
| Node-config/bootstrap/bundle root loading | `crates/daemon/ryeos-app/src/node_config/` and `crates/daemon/ryeos-bundle/src/installed.rs` |
| CLI command behavior | `crates/bin/cli/src/` plus bundle alias/verb descriptors |
| Offline command execution | `crates/bin/cli/src/offline_dispatch.rs` |
| Help output | `crates/bin/cli/src/help.rs` |
| Init/publish/sign/vault tooling | `crates/tools/core-tools/src/actions/` |
| Runtime protocol semantics | `crates/engine/ryeos-runtime/`, `crates/runtimes/*`, bundle protocol descriptors |

## Guardrails for agents

- Prefer changing the shared source of truth over adding CLI/app-specific
  mirrors of descriptor semantics.
- If descriptor resolution is needed, use engine APIs instead of manually
  opening kind-specific files.
- Keep `ryeos-api` generic; daemon composition should wire UI-specific pieces.
- After bundle or binary changes, refresh/sign bundles before trusting test
  failures.
