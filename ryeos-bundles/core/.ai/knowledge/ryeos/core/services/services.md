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
| `bundle/install`       | `bundle.install`      | `ryeos.execute.service.bundle/install` |
| `bundle/list`          | `bundle.list`         | none                                   |
| `bundle/remove`        | `bundle.remove`       | `ryeos.execute.service.bundle/remove`  |

Bundle install and remove are **offline-only** (daemon must be stopped).

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

| Service                | Endpoint              | Caps Required |
|------------------------|-----------------------|---------------|
| `scheduler/register`   | `scheduler.register`  | none          |
| `scheduler/list`       | `scheduler.list`      | none          |
| `scheduler/deregister` | `scheduler.deregister`| none          |
| `scheduler/pause`      | `scheduler.pause`     | none          |
| `scheduler/resume`     | `scheduler.resume`    | none          |
| `scheduler/show_fires` | `scheduler.show_fires`| none          |

### Core Services

| Service                | Endpoint              | Caps Required                          |
|------------------------|-----------------------|----------------------------------------|
| `fetch`                | `fetch`               | `ryeos.execute.service.fetch`          |
| `verify`               | `verify`              | `ryeos.execute.service.verify`         |
| `node-sign`            | `node-sign`           | `ryeos.execute.service.node-sign`      |
| `rebuild`              | `rebuild`             | `ryeos.execute.service.rebuild`        |
| `maintenance/gc`       | `maintenance.gc`      | `ryeos.execute.service.maintenance/gc` |
| `health/status`        | `health.status`       | none                                   |
| `identity/public_key`  | `identity.public_key` | none                                   |
| `system/status`        | `system.status`       | none                                   |
| `commands/submit`      | `commands.submit`     | `ryeos.execute.service.commands/submit`|

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
