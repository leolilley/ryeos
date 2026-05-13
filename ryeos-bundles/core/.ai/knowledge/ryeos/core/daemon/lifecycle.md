---
category: ryeos/core
tags: [operations, daemon, lifecycle, init, state]
version: "1.0.0"
description: >
  The daemon lifecycle — initialization, state directory structure,
  startup, shutdown, and bootstrap sequence.
---

# Daemon Lifecycle

The daemon (`ryeosd`) is the long-running process at the heart of
Rye OS. It holds the CAS, manages threads, serves HTTP, and
dispatches execution.

## Initialization (`ryeos init`)

Before first use, a node must be initialized:

```
ryeos init
```

This creates:
1. **Node identity** — Ed25519 key pair for the daemon
2. **State directory** — `<XDG_DATA_HOME>/ryeosd/<node-id>/`
3. **CAS store** — content-addressed storage for items and events
4. **Projection database** — `projection.sqlite3` for queries
5. **Bundle registry** — registers core and standard bundles

The node ID is derived from the public key fingerprint.

## State Directory Structure

```
<state_dir>/
├── .ai/
│   ├── bundles/
│   │   ├── core/           ← Core bundle (always present)
│   │   ├── standard/       ← Standard bundle (always present)
│   │   └── <custom>/       ← User-installed bundles
│   └── node/
│       ├── identity.yaml   ← Node key and metadata
│       └── config.yaml     ← Node-level config
├── cas/
│   └── <content-hash>...   ← Content-addressed objects
├── projection.sqlite3      ← Query database
└── threads/
    └── <thread-id>/
        ├── events.jsonl    ← Append-only event log
        └── state.json      ← Thread state snapshot
```

## Startup Sequence

When `ryeosd` starts:

1. **Load bundles** — scan registered bundle directories
2. **Bootstrap engine** — load kind schemas, parsers, handlers
   (Layer 1: raw signed-YAML for handlers/protocols, breaks chicken-and-egg)
3. **Build projection** — index all items into SQLite
4. **Register services** — wire up in-process service endpoints
5. **Start HTTP server** — bind to configured address
6. **Restore threads** — recover any interrupted threads
7. **Start scheduler** — evaluate registered schedules

## Shutdown

Graceful shutdown:
1. Stop accepting new requests
2. Wait for in-flight executions to complete (or timeout)
3. Persist thread state
4. Close projection database
5. Exit

## Bootstrap Chicken-and-Egg

The engine has a bootstrap ordering problem: parsers and handlers
are defined as items, but items need parsers to be read. Solution:
- **Layer 1:** Load handlers, protocols, and kind schemas as raw
  signed YAML (no composition, no extends chains)
- **Layer 2:** Use the Layer-1 handlers to parse everything else
  (directives, tools, knowledge, etc.)

This is why `handler` and `protocol` kinds use the `identity`
composer and the raw YAML parser.

## Health Monitoring

- `GET /health` — unauthenticated health check
- `ryeos status` — detailed status (version, uptime, threads, bundles)

## Offline Operations

Some operations require the daemon to be stopped:
- `bundle install` / `bundle remove`
- `rebuild`
- Direct CAS manipulation

Online operations (execute, fetch, sign, thread queries) require
the daemon to be running.
