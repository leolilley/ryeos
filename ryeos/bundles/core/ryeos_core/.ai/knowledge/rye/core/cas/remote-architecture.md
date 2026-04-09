<!-- rye:signed:2026-04-09T00:09:13Z:4ecb6561c200370e4b0a5d45b7d5cbe8f4ea1c71db6c36c2f8a3be7c5a1ea6f9:Fkg_Xg7RqXPu8pPPT-2mH9GtyTNsvz-mIlQI6GFr9wgR76OtKBhoMwH1NBLZH_U6cYWHBWU8C9XIZeqoGTdXDw:4b987fd4e40303ac -->
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

ryeos-node enables remote execution of tools and graphs by syncing
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
- **ProjectSnapshot** — point-in-time project state commit with parent lineage (like a git commit)

## Snapshot & Fold-Back

Each `/push` creates a `ProjectSnapshot` with parent chain. `/execute` creates
an execution snapshot and folds it back via three-way merge:

- Fast-forward if HEAD unchanged
- Three-way merge if HEAD moved (bounded retry with jitter)
- Conflict record stored on thread if unresolvable

## Webhook Execution

Webhook auth via `webhook_bindings` table:
- HMAC-SHA256 verification (`X-Webhook-Signature`, `X-Webhook-Timestamp`)
- Replay protection via `webhook_deliveries_replay` table
- Binding controls `item_type`, `item_id`, `project_path`
- Caller can only provide `parameters`

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
5. POST /push ────────────────→  Validate manifest graph
                                 Create ProjectSnapshot
                                 Advance HEAD (optimistic CAS)
6. POST /push/user-space ─────→  Push user space independently
7. POST /execute ─────────────→  Resolve HEAD snapshot
                                 Create mutable checkout
                                 Run executor
                                 Fold-back (three-way merge)
                     ←─────────  Return snapshot hash + result
8. POST /objects/get ─────────→  Fetch results
```

## Execution Flow (Server Side)

1. Dual auth (bearer API key OR webhook HMAC)
2. Resolve HEAD snapshot from `project_refs`
3. Create mutable checkout from snapshot cache
4. Cache user space from `user_space_refs`
5. Inject user secrets (validated against reserved names)
6. Wire ExecuteTool against checkout
7. Run executor (chain resolution → primitive execution)
8. Promote execution CAS objects to user CAS
9. Build post-execution manifest, compare to base
10. Fold-back: fast-forward or three-way merge into HEAD
11. Cleanup execution space

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

Remotes are configured in `.ai/config/remotes/remotes.yaml`:

```yaml
remotes:
  default:
    url: "https://ryeos--ryeos-node-remote-server.modal.run"
    key_env: "RYE_REMOTE_API_KEY"
```

`resolve_remote(name, project_path)` reads from config. No environment variable fallbacks — all remotes must be declared in config.

- `RYE_SIGNING_KEY_DIR` — remote's signing key directory (server-side)
- `project_path` replaces the old `project_name` field throughout the remote system
