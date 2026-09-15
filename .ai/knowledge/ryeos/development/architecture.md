<!-- ryeos:signed:2026-09-04T09:10:36Z:54a6c84c924312a5428b492e041fa7a8775120e702796c4d513d3246c3e03903:XmwrxhnSqdijvWSA26/ayoE3gmmrN14vvCX/nthypUUPkCpY9KrXqDgfxRZKa50lT1r0ocNCd1+loR9aySUSAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "architecture"
title: "Architecture Map"
description: "Short orientation for crates, bundles, execution flow, and trust boundaries"
entry_type: reference
version: "1.6.0"
```

# Architecture Map

Use this for orientation before changing code. It is intentionally a map, not a
full design document.

## Main crate map

| Area | Path | Owns |
|---|---|---|
| Kernel primitives | `crates/kernel/lillux/` | process lifecycle/type state, descriptor and exact-process authority, native Linux sandbox mechanics, durable filesystem/CAS operations, Ed25519/X25519/SHA primitives |
| Engine | `crates/engine/ryeos-engine/` | item resolution, trust verification, composition, kind-schema projections, plans, immutable node isolation policy |
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
| Local inference | `bundles/local-inference/` | optional local-model workers, provider routes, activation fixtures, and acceptance probes |

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
  -> serialized plan freezes any kind-schema-projected execution ceilings
  -> plan, parent launch, and node isolation ceilings are intersected
  -> Lillux creates the exact target awaiting durable attachment
  -> daemon persists process ownership and authorizes release
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
- Isolation activation and OS controls come only from the `isolation` member
  of the complete signed generation at
  `<app-root>/.ai/node/policies/isolation.yaml`; items cannot override the
  compiled node-policy snapshot.
- The isolation-adapter wire is a clean-cut v4 contract. It admits a
  self-contained exact adapter, an explicit PID-namespace choice, and a
  bounded sorted collection of protected target channels. Core publishes the
  generic `linux-lillux` backend declaration, but ordinary init profiles keep
  isolation disabled until a dedicated signed development profile is
  deliberately added. Direct and adapter launches preserve the same exact
  target descriptor/environment bindings, while parent protocol owners retain
  typed Lillux byte streams rather than raw Unix sockets.

See `knowledge:ryeos/core/node/execution-isolation` for node-owned confinement
and `knowledge:ryeos/core/execution/attachment-before-execution` for the
orthogonal durable process lifecycle.

## Project AI deployable surfaces

AI-only project sync is a typed deploy pipeline, not a broad `.ai/` copy.
`ryeos-state::project_sync` classifies project paths into deployable surfaces,
node-local/runtime-owned prefixes, unknown `.ai` paths, and non-`.ai` content.

Current deployable project surfaces include item/config/trust content, project
schedule declarations under `.ai/config/schedules`, and project-authored node
extension declarations such as `.ai/node/engine/kinds` and `.ai/node/verbs`.

What must NOT deploy is split into two reason-named code constants, both
enforced as a **scope-independent structural floor** — they apply to
`full_project` sync as well as `ai_only` and are not ignore policy:

- `NEVER_DEPLOY_SECRETS` — credentials: `.ai/node/identity`, `.ai/node/auth`,
  `.ai/node/vault`, `.ai/config/keys/signing`.
- `NODE_OWNED` — node runtime state and transaction anchors: `.ai/state`,
  `.ai/cache`, `.ai/.bundles.lock`, `.ai/node/schedules`, `.ai/node/routes`,
  `.ai/node/bundles`.

Classification order is `never_deploy_secrets → ignore → node_owned →
deployable → unknown/non-ai`, so an ignored file inside a deployable surface
(e.g. `.ai/tools/x/__pycache__/y.pyc`) is dropped, not shipped. The ingest
ignore policy (`.ai/node/policies/ingest_ignore.yaml`) is the complete source of
conventional ignore patterns; the engine contributes no hidden defaults. It
supports **path-anchored** patterns (e.g. `/.ai/config/remotes/`, which shipped
profiles select because a project's remotes config is environment-specific and
must not travel). It is a member of the complete node-signed policy generation
and is changed through the stopped-node policy workflow, not edited in place.

`ryeos init` writes a generated, **read-only** `.ai/node/sync/policy.yaml` that
documents the effective policy — deployable surfaces, both floors, and a pointer
to the signed ignore-policy source. It is a discovery window, not a control surface:
floors and surfaces are enforced in code, and editing the file changes nothing.
Source: `crates/state/ryeos-state/src/{project_sync,ignore}.rs`.

`project.apply-snapshot` materializes an AI-only snapshot to staging, builds a
`ryeos-api::project_deploy` plan from staged intent, swaps managed project
surfaces, prepares runtime projections, advances the deployed ref, and only
then finalizes backups. A surface is either one exact regular file (including
the root source/generated manifests) or a complete directory subtree. If
schedule projection or ref advancement fails during the request, prepared
schedule YAML/DB mutations and every file/directory surface swap are rolled
back.

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

The scheduler runtime gate in `AppState` serializes the deploy mutation window
against timer and recovery dispatch. Mutation services take the write side for
plan -> prepare-commit -> ref-advance (plus rollbacks); timer/recovery dispatch
take the read side and skip/wait while that window is held. Request validation,
CAS reads, and staging materialization run before the gate, so dispatch is not
blocked behind work that scales with project size (per-project serialization is
the apply lock's job).

## Where to change things

| Need | Likely area |
|---|---|
| Item resolution/composition behavior | `crates/engine/ryeos-engine/src/` |
| Operational node config/bootstrap and bundle root loading | `crates/daemon/ryeos-app/src/node_config/` and `crates/daemon/ryeos-bundle/src/installed.rs` |
| Node-owned semantic policy and complete policy generation | `crates/daemon/ryeos-app/src/node_policy/` |
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
- Raw namespace, mount, pivot-root, seccomp, descriptor, pidfd, procfs,
  signal, and bounded process-settle mechanics belong in Lillux. Engine and
  daemon layers carry only typed launch/process authorities and signed RyeOS
  policy vocabulary.
- Process-scoped artifact-generation naming, permissions, advisory lifetime
  locking, stale collection, and descriptor-relative teardown likewise belong
  in Lillux. Engine code owns artifact hashes, quotas, and semantic use only.
- A per-execution control such as network denial is a signed mechanical
  projection in the kind schema, frozen into `ExecutionPlan`, then
  irreversibly intersected with parent/node authority. Do not add tool-name,
  compiler, provider, or project branches to dispatch.
- Signed Tool configuration may map schema-validated scalar `params` through
  the existing bounded rye-expr/1 template context into individual argv or
  environment values. Each rendered argv value remains one argument. Reuse
  that data path before inventing a tool-specific argument builder, shell/JSON
  shim, or command-multiplexer binary.
- Every executable Tool explicitly selects its subprocess protocol through the
  Tool kind's closed signed allowlist. Use callback-free `opaque` for an
  untrusted build/test executable; selecting `tool_callback` is an explicit
  bearer grant, not a default inferred from the Tool kind or command name.
- If descriptor resolution is needed, use engine APIs instead of manually
  opening kind-specific files.
- Managed launch preparation owns only the managed program being launched
  now. A callback/borrowed child shares the exact request engine, project
  authority, workspace and lifeline through `ExecutionProvenance`, but it is
  otherwise an ordinary RyeOS execution. Its own effective program must own
  its command, external realizations, process environment, effects, limits and
  capsule. Do not add deferred child environments or child-context targets to
  the parent launch-preparer protocol/capsule.
- Parent outer-program realization inheritance may supply the sealed byte view
  used while verifying a child plan when that child declares no replacement
  realization set. That preserves the execution view already admitted for an
  ordinary composing parent; it does not import content retained only inside a
  managed runtime's prepared launch. A child that declares realizations owns
  its complete replacement set, and an inherited-only entry is never child
  command or executable authority.
- Keep `ryeos-api` generic; daemon composition should wire UI-specific pieces.
- After bundle or binary changes, refresh/sign bundles before trusting test
  failures.
