<!-- ryeos:signed:2026-05-22T03:35:36Z:7050dc3db5de7526487c4b762f571cc9ef88dbb1affbbebd4161dc881d47a262:YEnErwghmzyU915NbwPnD/ZJnEuoBx/BDtcG7lbda7IBEIuFAzdrmanlNGs4EGswpVfC1yMXiJCEmyJLg/TiCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, cli, verbs, aliases]
version: "2.0.0"
description: >
  Complete reference for the ryeos CLI — all verbs, aliases,
  and their arguments.
---

# CLI Reference

The `ryeos` CLI communicates with the daemon via HTTP. Commands are
dispatched through **verbs** (full names) and **aliases** (shortcuts).
Core contributes engine/control-plane verbs; standard contributes workflow
verbs such as threads, events, scheduler, commands, and compose.

## Setup

### `ryeos init`
Bootstrap user + node keys, discover and install bundles from a source
directory, pin publisher keys. Must be run before starting the daemon.

```
ryeos init [--source <dir>] [--system-space-dir <dir>] [--user-root <dir>]
           [--trust-file <file>...]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--source` | `/usr/share/ryeos` | Directory containing bundle subdirectories |
| `--system-space-dir` | XDG data dir / ryeos | Daemon state and installed bundles |
| `--user-root` | `~/.ryeos` | User space root (parent of `.ai/`) |
| `--trust-file` | (none) | Additional publisher trust docs to pin (repeatable) |

Init is **idempotent** — running it again preserves keys, atomically
updates bundles, and re-validates registrations.

Development usage:
```bash
ryeos init --source bundles --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

### `ryeos trust pin`
Pin a publisher's Ed25519 public key into the operator trust store.

```
ryeos trust pin --from <PUBLISHER_TRUST.toml>
```

Required before installing bundles from a third-party publisher.

## Core Verbs

### `ryeos execute <ref> [params...]`
Execute an item by canonical ref. Dispatches through the kind system
to the appropriate runtime.

### `ryeos fetch <ref>`
Resolve and read an item. Returns parsed metadata and optional content.
Does not execute.

- `--with-content` — include file body
- `--verify` — also verify the signature

Aliases: `f`

### `ryeos sign <ref>`
Cryptographically sign an item. Updates the signature header in-place.

- Supports glob patterns: `directive:*`, `tool:my/project/*`
- `--source project|user` — which space to sign in

Aliases: `s`

### `ryeos verify <ref>`
Verify an item's signature and integrity. Checks content hash,
key trust, and path anchoring.

### `ryeos status`
Show daemon status: version, uptime, bind address, active threads.

## Bundle Management

### `ryeos bundle install <path>`
Install a bundle into the daemon's state directory. Offline-only
(daemon must be stopped). Verifies signatures during install.

### `ryeos bundle list`
List all installed bundles with their names and source paths.

### `ryeos bundle remove <name>`
Remove an installed bundle. Offline-only.

## Thread Operations

Thread verbs are contributed by the standard bundle.

### `ryeos thread list`
List all threads. Optional `--limit` flag.

### `ryeos thread get <id>`
Get a single thread's detail: status, result, artifacts, facets.

### `ryeos thread tail <id>`
Tail a thread's events in real-time (SSE stream).

### `ryeos thread children <id>`
List direct child threads of a parent.

### `ryeos thread chain <id>`
Get the full parent chain (thread tree + edges).

## Event Operations

Event verbs are contributed by the standard bundle.

### `ryeos events replay <thread_id>`
Replay persisted events for a single thread.

- `--after-chain-seq <n>` — start after sequence number
- `--limit <n>` — max events to return

### `ryeos events chain-replay <chain-root-id>`
Replay events across an entire chain (root + all descendants).

## Scheduler Operations

Scheduler verbs are contributed by the standard bundle.

### `ryeos scheduler register`
Create or update a schedule spec.

### `ryeos scheduler list`
List all registered schedules.

### `ryeos scheduler deregister`
Remove a schedule spec.

### `ryeos scheduler pause` / `ryeos scheduler resume`
Pause or resume a schedule.

### `ryeos scheduler show-fires`
Show fire history for a schedule.

## Maintenance

### `ryeos rebuild`
Rebuild the daemon's projection database from CAS state. Offline-only.
`--verify` to also check signatures during rebuild.

### `ryeos maintenance gc`
Run garbage collection on CAS objects. Supports `--dry-run` and
`--compact` flags.

### `ryeos identity public-key`
Print the daemon's node identity public key.

## Remote Operations

Remote verbs are core daemon-control verbs for cross-node transfer,
execution, authorization, thread inspection, live bundle install, and
vault proxy operations.

See [Remote Command Reference](../remote/remote-command-reference.md)
for syntax, examples, required local capabilities, remote authorized-key
scopes, HTTP routes, outputs, and failure modes.

Quick list:

- `ryeos remote configure --remote <name> --url <url>`
- `ryeos remote list`
- `ryeos remote status --remote <name>`
- `ryeos remote authorize --remote <name> --public-key <key> --label <label> --scopes <cap>`
- `ryeos remote push --remote <name> --project <abs-path>`
- `ryeos remote pull --remote <name> --hashes <hash>... [--output-dir <dir>]`
- `ryeos remote execute --remote <name> --item-ref <ref> (--project <abs-path> | --no-project)`
- `ryeos remote threads --remote <name> [--limit <n>]`
- `ryeos remote thread-status --remote <name> --thread-id <id>`
- `ryeos remote bundle-install --remote <name> --bundle-name <bundle>`
- `ryeos remote vault-set --remote <name> --name <key> --value <value>`
- `ryeos remote vault-list --remote <name>`
- `ryeos remote vault-delete --remote <name> --name <key>`

## Vault Operations

Core vault verbs are `ryeos vault set`, `ryeos vault list`, and
`ryeos vault delete`. They operate on sealed daemon vault state and are
separate from runtime vault bindings resolved during execution preflight.

## Workflow Utilities

### `ryeos commands submit <thread_id> <command_type>`
Submit a runtime command (cancel, kill, interrupt, continue) to an
active thread.

### `ryeos compose`
Compose a knowledge graph context block within a token budget.

## Aliases Quick Reference

| Alias | Verb             |
|-------|------------------|
| `f`   | `fetch`          |
| `s`   | `sign`           |
