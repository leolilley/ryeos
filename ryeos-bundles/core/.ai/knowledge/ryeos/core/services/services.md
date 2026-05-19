---
category: ryeos/core
tags: [reference, services, daemon, in-process]
version: "1.0.0"
description: >
  The in-process service layer — all daemon services,
  their endpoints, and capabilities.
---

# Services

Services are in-process daemon endpoints. Unlike tools (subprocess),
services run inside the daemon process with no spawn overhead.

## Service Categories

### Bundle Services
Manage installed bundles.

| Service                | Endpoint              | Caps Required                          |
|------------------------|-----------------------|----------------------------------------|
| `bundle/install`       | `bundle.install`      | `ryeos.execute.service.bundle.install` |
| `bundle/export`        | `bundle.export`       | `ryeos.execute.service.bundle.export`  |
| `bundle/list`          | `bundle.list`         | none                                   |
| `bundle/remove`        | `bundle.remove`       | `ryeos.execute.service.bundle.remove`  |

Bundle install and remove are **offline-only** (daemon must be stopped).
Bundle export is **daemon-only** — walks the bundle tree and ingests
files into the node's CAS for cross-node transfer.

### Thread Services
Query and manage execution threads.

| Service                | Endpoint              | Caps Required |
|------------------------|-----------------------|---------------|
| `threads/list`         | `threads.list`        | none          |
| `threads/get`          | `threads.get`         | none          |
| `threads/children`     | `threads.children`    | none          |
| `threads/chain`        | `threads.chain`       | none          |

### Event Services
Replay persisted thread events.

| Service                | Endpoint              | Caps Required |
|------------------------|-----------------------|---------------|
| `events/replay`        | `events.replay`       | none          |
| `events/chain_replay`  | `events.chain_replay` | none          |

### Scheduler Services
CRUD operations for scheduled executions.

| Service                | Endpoint              | Caps Required                                    |
|------------------------|-----------------------|--------------------------------------------------|
| `scheduler/register`   | `scheduler.register`  | `ryeos.execute.service.scheduler.register`       |
| `scheduler/list`       | `scheduler.list`      | `ryeos.execute.service.scheduler/list`           |
| `scheduler/deregister` | `scheduler.deregister`| `ryeos.execute.service.scheduler.deregister`     |
| `scheduler/pause`      | `scheduler.pause`     | `ryeos.execute.service.scheduler.pause`          |
| `scheduler/resume`     | `scheduler.resume`    | `ryeos.execute.service.scheduler.resume`         |
| `scheduler/show_fires` | `scheduler.show_fires`| `ryeos.execute.service.scheduler/show_fires`     |

### Core Services

| Service                | Endpoint              | Caps Required                          |
|------------------------|-----------------------|----------------------------------------|
| `fetch`                | `fetch`               | `ryeos.execute.service.fetch`          |
| `verify`               | `verify`              | `ryeos.execute.service.verify`         |
| `node-sign`            | `node-sign`           | `ryeos.execute.service.node_sign`      |
| `rebuild`              | `rebuild`             | `ryeos.execute.service.rebuild`        |
| `maintenance/gc`       | `maintenance.gc`      | `ryeos.execute.service.maintenance.gc` |
| `health/status`        | `health.status`       | none                                   |
| `identity/public_key`  | `identity.public_key` | none                                   |
| `system/status`        | `system.status`       | none                                   |
| `ingest/ignore`        | `ingest.ignore`       | none                                   |
| `commands/submit`      | `commands.submit`     | `ryeos.execute.service.commands.submit`|

### Object Services
CAS object operations.

| Service                | Endpoint              | Caps Required                          |
|------------------------|-----------------------|----------------------------------------|
| `objects/has`          | `objects.has`         | `ryeos.execute.service.objects.has`    |
| `objects/put`          | `objects.put`         | `ryeos.execute.service.objects.put`    |
| `objects/get`          | `objects.get`         | `ryeos.execute.service.objects.get`    |

### Vault Services
Sealed secret operations scoped to the caller's fingerprint.

| Service                | Endpoint              | Caps Required                          |
|------------------------|-----------------------|----------------------------------------|
| `vault/set`            | `vault.set`           | `ryeos.execute.service.vault.set`      |
| `vault/list`           | `vault.list`          | `ryeos.execute.service.vault.list`     |
| `vault/delete`         | `vault.delete`        | `ryeos.execute.service.vault.delete`   |

### Remote Services
Cross-node operations. All are **daemon-only**.

| Service                | Endpoint              | Caps Required                          |
|------------------------|-----------------------|----------------------------------------|
| `remote/configure`     | `remote.configure`    | `ryeos.execute.service.remote.configure`|
| `remote/list`          | `remote.list`         | `ryeos.execute.service.remote.list`    |
| `remote/status`        | `remote.status`       | `ryeos.execute.service.remote.status`  |
| `remote/push`          | `remote.push`         | `ryeos.execute.service.remote.push`    |
| `remote/pull`          | `remote.pull`         | `ryeos.execute.service.objects.get`    |
| `remote/execute`       | `remote.execute`      | `ryeos.execute.service.remote.admin` |
| `remote/authorize`     | `remote.authorize`    | `ryeos.execute.service.authorize.key`  |
| `remote/threads`       | `remote.threads`      | `ryeos.execute.service.remote.threads` |
| `remote/thread-status` | `remote.thread_status`| `ryeos.execute.service.remote.thread-status`|
| `remote/bundle-install`| `remote.bundle_install`| `ryeos.execute.service.bundle.install`|
| `remote/vault-set`     | `remote.vault_set`    | `ryeos.execute.service.vault.set`      |
| `remote/vault-list`    | `remote.vault_list`   | `ryeos.execute.service.vault.list`     |
| `remote/vault-delete`  | `remote.vault_delete` | `ryeos.execute.service.vault.delete`   |

`remote/pull` fetches CAS objects by hash from a remote node. Fail-closed:
aborts if any requested hash is missing.

`remote/bundle-install` installs a bundle from a remote via CAS pipeline.
Fail-closed: aborts if any blob is missing, cleans up partial installs,
runs preflight before registering.

### Identity Services

| Service                | Endpoint              | Caps Required                          |
|------------------------|-----------------------|----------------------------------------|
| `authorize-key`        | `authorize_key.set`   | `ryeos.execute.service.authorize.key`  |

## Service vs Tool

| Aspect        | Service                 | Tool                      |
|---------------|-------------------------|---------------------------|
| Execution     | In-process              | Subprocess                |
| Overhead      | Minimal                 | Fork + exec               |
| Isolation     | Shared daemon memory    | Separate process          |
| Protocol      | Direct function call    | Wire protocol             |
| Use case      | Daemons ops, queries    | File ops, external commands|

Services are best for daemon-internal operations (thread queries,
bundle management, health checks). Tools are best for operations
that need process isolation (file system, network, shell commands).
