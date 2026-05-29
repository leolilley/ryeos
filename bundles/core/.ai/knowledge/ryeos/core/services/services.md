<!-- ryeos:signed:2026-05-23T04:53:02Z:afce39dd7bcbe39498e9be0cfc53839394c641b067411ac88c16f1ab089c0250:zcok8SQ+E9sV5eAEsLGdRwhRHmgMLl4yBW4D8ZqkPEGuZI0dG83swKn02sUsU1aVHzzXD7MZ5esZzi98wumpAw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->

---
category: ryeos/core
tags: [reference, services, daemon, in-process]
version: "1.2.0"
description: >
  The in-process service layer — daemon services, endpoints,
  capabilities, and exposure modes.
---

# Services

Services are in-process daemon endpoints. Unlike tools, services run
inside the daemon process with no subprocess spawn overhead. A service
can be exposed through `/execute`, through a dedicated HTTP route,
through a CLI verb/alias, or through multiple surfaces at once.

Service descriptors live under `.ai/services/`; the Rust handlers live
under `crates/daemon/ryeos-api/src/handlers/` and export `DESCRIPTOR`
records consumed by the daemon registry.

## Bundle Services

| Service | Endpoint | Caps Required |
|---|---|---|
| `bundle/install` | `bundle.install` | `ryeos.execute.service.bundle.install` |
| `bundle/export` | `bundle.export` | `ryeos.execute.service.bundle.export` |
| `bundle/list` | `bundle.list` | none |
| `bundle/remove` | `bundle.remove` | `ryeos.execute.service.bundle.remove` |

Local bundle install/remove are offline-only. `bundle/export` is
daemon-only and is used by remote bundle installation to export bundle
file hashes through CAS.

## Core System Services

| Service | Endpoint | Caps Required |
|---|---|---|
| `fetch` | `fetch` | `ryeos.execute.service.fetch` |
| `verify` | `verify` | `ryeos.execute.service.verify` |
| `node-sign` | `node-sign` | `ryeos.execute.service.node_sign` |
| `rebuild` | `rebuild` | `ryeos.execute.service.rebuild` |
| `maintenance/gc` | `maintenance.gc` | `ryeos.execute.service.maintenance.gc` |
| `health/status` | `health.status` | none |
| `identity/public_key` | `identity.public_key` | none |
| `identity/authorize-key` | `identity.authorize-key` | `ryeos.execute.service.authorize.key` |
| `system/status` | `system.status` | none |
| `system/ingest-ignore` | `system.ingest-ignore` | none |
| `system/push-head` | `system.push-head` | `ryeos.execute.service.push.head` |

`health/status`, `identity/public_key`, and `system/ingest-ignore` back
unauthenticated discovery routes. Mutating routes such as
`identity/authorize-key` and `system/push-head` require signed auth plus
the listed capability.

## Object Services

| Service | Endpoint | Caps Required |
|---|---|---|
| `objects/has` | `objects.has` | `ryeos.execute.service.objects.has` |
| `objects/put` | `objects.put` | `ryeos.execute.service.objects.put` |
| `objects/get` | `objects.get` | `ryeos.execute.service.objects.get` |

Object services read and write the node CAS. Remote push/pull, pushed
HEAD execution, and remote bundle install all depend on these services.
Object fetch flows fail closed when requested hashes are missing.

## Vault Services

| Service | Endpoint | Caps Required |
|---|---|---|
| `vault/set` | `vault.set` | `ryeos.execute.service.vault.set` |
| `vault/list` | `vault.list` | `ryeos.execute.service.vault.list` |
| `vault/delete` | `vault.delete` | `ryeos.execute.service.vault.delete` |

Vault services mutate or read the node vault. In v1, vault storage is a
single node-level store protected by capabilities, not a per-principal
namespace.

## Remote Services

Remote services are daemon-only local orchestrators. They may call
unauthenticated discovery routes, signed routes on a target daemon, or
both. Keep local service caps separate from remote authorized-key scopes;
see [Remote Command Reference](../remote/remote-command-reference.md)
for the full matrix.

| Service | Endpoint | Local Caps Required |
|---|---|---|
| `remote/configure` | `remote.configure` | `ryeos.execute.service.remote.configure` |
| `remote/list` | `remote.list` | `ryeos.execute.service.remote.list` |
| `remote/status` | `remote.status` | `ryeos.execute.service.remote.status` |
| `remote/doctor` | `remote.doctor` | `ryeos.execute.service.remote.doctor` |
| `remote/push` | `remote.push` | `ryeos.execute.service.remote.push` |
| `remote/pull` | `remote.pull` | `ryeos.execute.service.objects.get` |
| `remote/execute` | `remote.execute` | `ryeos.execute.service.remote.admin` |
| `remote/authorize` | `remote.authorize` | `ryeos.execute.service.remote.admin` |
| `remote/threads` | `remote.threads` | `ryeos.execute.service.remote.admin` |
| `remote/thread-status` | `remote.thread-status` | `ryeos.execute.service.remote.admin` |
| `remote/bundle-install` | `remote.bundle-install` | `ryeos.execute.service.bundle.install` |
| `remote/vault-set` | `remote.vault-set` | `ryeos.execute.service.remote.admin` |
| `remote/vault-list` | `remote.vault-list` | `ryeos.execute.service.remote.admin` |
| `remote/vault-delete` | `remote.vault-delete` | `ryeos.execute.service.remote.admin` |

## Standard Thread/Event/Scheduler Services

These are contributed by the standard bundle.

| Service | Endpoint | Caps Required |
|---|---|---|
| `threads/list` | `threads.list` | none |
| `threads/get` | `threads.get` | none |
| `threads/children` | `threads.children` | none |
| `threads/chain` | `threads.chain` | none |
| `threads/cancel` | `threads.cancel` | route-backed control surface |
| `events/replay` | `events.replay` | none |
| `events/chain_replay` | `events.chain_replay` | none |
| `commands/submit` | `commands.submit` | `ryeos.execute.service.commands.submit` |
| `scheduler/register` | `scheduler.register` | `ryeos.execute.service.scheduler.register` |
| `scheduler/list` | `scheduler.list` | `ryeos.execute.service.scheduler.list` |
| `scheduler/deregister` | `scheduler.deregister` | `ryeos.execute.service.scheduler.deregister` |
| `scheduler/pause` | `scheduler.pause` | `ryeos.execute.service.scheduler.pause` |
| `scheduler/resume` | `scheduler.resume` | `ryeos.execute.service.scheduler.resume` |
| `scheduler/show_fires` | `scheduler.show_fires` | `ryeos.execute.service.scheduler/show_fires` |

## Service vs Tool

| Aspect | Service | Tool |
|---|---|---|
| Execution | In-process | Subprocess |
| Overhead | Minimal | Fork + exec |
| Isolation | Shared daemon memory | Separate process |
| Protocol | Direct function call | Runtime/tool wire protocol |
| Best for | Daemon ops, queries, orchestration | External binaries, file/network/shell work |

## Exposure modes

### Route-backed services

Route-backed services have a descriptor under `.ai/services/` and an
HTTP route under `.ai/node/routes/`. Examples:

- `service:system/push-head` via `/push-head`
- `service:objects/get` via `/objects/get`
- `service:threads/list` via `/threads`

### Verb-backed services

Verb-backed services have a descriptor plus a verb/alias entry for CLI
or node-command invocation. Examples:

- `service:bundle/install` via `bundle install`
- `service:remote/execute` via `remote execute`
- `service:vault/set` via `vault set`

Verb descriptors live under `.ai/node/verbs/`; aliases live under
`.ai/node/aliases/`.

### Execute-only services

Some services are primarily invoked by canonical ref through `/execute`
or by internal code. They still need signed service descriptors and Rust
`ServiceDescriptor` records, but they do not need a dedicated route or
alias.
