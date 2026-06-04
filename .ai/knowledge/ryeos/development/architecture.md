<!-- ryeos:signed:2026-06-04T02:13:35Z:b60e24e02e52f26cd8f801c092f0d2a13dea0b9ddc9e939cf9f938ecaaf8a2cb:h/i9mVtoNY46cXU73280nyTeBcnV+W8J+9dBWcrDAlEOOoC4aX1KY4wLROEu1aEUEikCHiDwjqtqiztqV2S7DA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
| API services | `crates/daemon/ryeos-api/` | service handlers, route dispatch, remote sync, project deploy reconciliation |
| Scheduler | `crates/state/ryeos-scheduler/` | schedule projection, planning, timer dispatch, fire history rebuild |
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

## Project AI deployable surfaces

AI-only project sync is a typed deploy pipeline, not a broad `.ai/` copy.
`ryeos-state::project_sync` classifies project paths into deployable surfaces,
node-local/runtime-owned prefixes, unknown `.ai` paths, and non-`.ai` content.

Current deployable project surfaces include item/config/trust content, project
schedule declarations under `.ai/config/schedules`, and project-authored node
extension declarations such as `.ai/node/engine/kinds` and `.ai/node/verbs`.
Node-owned runtime paths such as `.ai/node/schedules`, `.ai/node/routes`,
`.ai/state`, and signing keys remain local-only and must fail closed if a
project snapshot tries to deploy them.

`project.apply-snapshot` materializes an AI-only snapshot to staging, builds a
`ryeos-api::project_deploy` plan from staged intent, swaps managed project roots,
prepares runtime projections, advances the deployed ref, and only then finalizes
backups. If schedule projection or ref advancement fails during the request,
prepared schedule YAML/DB mutations and root swaps are rolled back.

### Project schedule declarations

Schedules use a two-surface model:

```text
project intent                         node-owned runtime projection
.ai/config/schedules/*.yaml   ───────▶ <system_space>/.ai/node/schedules/*.yaml
```

Project declarations are validated as intent. They may request schedule fields
such as `schedule_id`, `item_ref`, `schedule_type`, `expression`, policies,
`enabled`, and object `params`, but they must not declare node-owned execution
authority. Runtime schedule specs are generated and node-signed with:

- `execution.requester_fingerprint` and `execution.capabilities` from the
  verified deploy caller on create;
- preserved execution requester/capabilities on project-managed update;
- `managed_by.type: project_ai_sync` metadata containing project key/root and
  source path/hash.

Project schedule declaration signature/trust verification is deferred. Deploy
admission validates declaration shape and derives runtime authority from the
verified deploy caller; generated runtime specs are node-signed and verified on
rebuild.

Manual schedule ID collisions are not adopted automatically. Project sync fails
closed until an operator deregisters/renames the manual schedule or a future
explicit adoption path is implemented. Removing a project-managed declaration
removes the active node schedule spec and DB projection while preserving fire
history under `.ai/state/schedules`.

The scheduler runtime gate in `AppState` serializes project/scheduler mutations
against timer and recovery dispatch. Mutation services take the write side;
timer/recovery dispatch take the read side and skip/wait while a deploy is in
progress.

## Where to change things

| Need | Likely area |
|---|---|
| Item resolution/composition behavior | `crates/engine/ryeos-engine/src/` |
| Node-config/bootstrap/bundle root loading | `crates/daemon/ryeos-app/src/node_config/` and `crates/daemon/ryeos-bundle/src/installed.rs` |
| Project AI sync/deploy surfaces | `crates/state/ryeos-state/src/project_sync.rs` and `crates/daemon/ryeos-api/src/project_deploy/` |
| Schedule runtime behavior | `crates/state/ryeos-scheduler/` plus scheduler service handlers in `crates/daemon/ryeos-api/src/handlers/` |
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
