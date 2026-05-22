<!-- ryeos:signed:2026-05-22T04:30:06Z:f10af9474c040c35202a50bf4bd8ade2f17bb9fbd1b287b18047ba5cfeb7eefd:w9DDPLPUkpGc4aZHbpVapwMBG9xUFAP+7A1tK8JIlqzJVoH22ZOF7NYEn5RTfIqXMJf5W/Tq8gSSeYGz/iy6CA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

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
1. **User identity** — Ed25519 key pair used by the CLI
2. **Node identity** — Ed25519 key pair used by the runtime
3. **System space** — default `<XDG_DATA_HOME>/ryeos/`
4. **CAS state** — content-addressed storage for items and events
5. **Bundle registrations** — signed records for installed bundles

The node ID is derived from the public key fingerprint.

## State Directory Structure

```
<system-space>/
├── .ai/
│   ├── bundles/
│   │   ├── core/           ← Core bundle
│   │   ├── standard/       ← Standard bundle
│   │   └── <custom>/       ← User-installed bundles
│   └── node/
│       ├── identity/       ← Node signing keys (private_key.pem, public-identity.json)
│       └── bundles/        ← Signed bundle registration records
└── state/
    ├── objects/            ← Content-addressed objects
    ├── refs/               ← CAS refs
    └── runtime.sqlite3     ← Runtime projection database
```

## Startup Sequence

When the runtime process starts:

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
