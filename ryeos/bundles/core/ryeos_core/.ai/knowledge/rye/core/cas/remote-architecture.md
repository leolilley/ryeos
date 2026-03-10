<!-- rye:signed:2026-03-09T23:48:24Z:d47d3182d4dd671ef6f671c1add6b9093e10f168bf4b16f648624598f43d36cf:IDTItjme85agJIQ_HIgAhtEB-hSq7bgm4WZdXWqWwrLHtSoPAqpft2K2CuEyT6jinXPJrESPuzeBgc1JpV9dBQ==:4b987fd4e40303ac -->
```yaml
name: remote-architecture
title: CAS Remote Architecture
entry_type: reference
category: rye/core/cas
version: "1.0.0"
author: rye-os
created_at: 2026-03-10T00:00:00Z
tags:
  - cas
  - remote
  - sync
  - execution
```

# CAS Remote Architecture

## Overview

ryeos-remote enables remote execution of tools and graphs by syncing
project state via a Content-Addressable Store (CAS). Objects are
addressed by their SHA-256 hash — syncing is efficient because
identical content is never transferred twice.

## Object Model

All data flows through the CAS as immutable, hash-addressed objects:

- **Blobs** — raw file content (`.py`, `.md`, `.yaml` files)
- **ItemSource** — versioned snapshot of a `.ai/` file with integrity + signature
- **SourceManifest** — filesystem closure mapping paths to object hashes
- **ExecutionSnapshot** — immutable run checkpoint with manifest refs + result refs

See `rye/core/cas/object-kinds` knowledge entry for full object kind reference.

## Sync Protocol

```
LOCAL                              REMOTE
─────                              ──────
1. Build project + user manifests
2. Collect transitive object set
3. POST /objects/has ─────────→  Check user CAS
                     ←─────────  Return missing[]
4. POST /objects/put ─────────→  Store in user CAS
   (only missing objects)        Verify hashes
5. POST /execute ─────────────→  Materialize + run
                     ←─────────  Return snapshot hash
6. POST /objects/get ─────────→  Fetch results
```

## Execution Flow (Server Side)

1. Auth (API key or JWT)
2. Validate system_version (reject major/minor mismatch)
3. Materialize temp project + user space from manifest hashes
4. Wire ExecuteTool against materialized paths
5. Run executor (chain resolution → primitive execution)
6. Store ExecutionSnapshot in user CAS
7. Cleanup temp dirs
8. Return snapshot hash + result

## Security

- Objects are hash-verified on upload (PUT rejects mismatches)
- User CAS is isolated per user_id (`/cas/{user_id}/`)
- Remote signs produced artifacts with its own Ed25519 key
- Secrets are injected as env vars per-request, never stored in CAS
- System version pinning prevents running against incompatible runtimes

## Tools

- `rye/core/remote/remote` — push, pull, status, execute actions
- Directives: init, push, execute, secrets

## Configuration

- `RYE_REMOTE_URL` — remote server URL
- `RYE_REMOTE_API_KEY` — authentication key
- `RYE_SIGNING_KEY_DIR` — remote's signing key directory (server-side)
