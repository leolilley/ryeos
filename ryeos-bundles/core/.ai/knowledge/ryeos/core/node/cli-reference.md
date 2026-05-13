---
category: ryeos/core
tags: [reference, cli, verbs, aliases]
version: "1.0.0"
description: >
  Complete reference for the ryeos CLI — all verbs, aliases,
  and their arguments.
---

# CLI Reference

The `ryeos` CLI communicates with the daemon via HTTP. Commands are
dispatched through **verbs** (full names) and **aliases** (shortcuts).

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

### `ryeos events replay <thread_id>`
Replay persisted events for a single thread.

- `--after-chain-seq <n>` — start after sequence number
- `--limit <n>` — max events to return

### `ryeos events chain-replay <chain-root-id>`
Replay events across an entire chain (root + all descendants).

## Scheduler Operations

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
