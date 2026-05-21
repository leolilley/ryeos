# ryeos:signed:2026-05-20T11:23:00Z:c7006a1655b598a4925ffa98c7a45ab2058a24f18caecfdccc553b6188fdc404:uJgfAS9M44uOqVd9NpMI1zvMbnTQqM0WfcyKo8u1zvsH1CmQVvQGAuS3PfvQsj252fGUzY8jW4jbZN1MyF0wDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea

---
category: ryeos/core/state
tags: [architecture, snapshots, dag, versioning, gc, compact]
version: "1.0.0"
description: >
  DAG-based snapshot versioning — how project snapshots form a directed
  acyclic graph, topological compaction, and the retention policy that
  drives GC.
---

# DAG Versioning

Project snapshots in Rye OS are not linear — they form a **directed
acyclic graph (DAG)**. This naturally supports merge histories where
multiple actors can push to the same project from different nodes.

## Snapshot Structure

Each snapshot carries a `parent_hashes: Vec<String>` field — a list of
one or more parent snapshot hashes. A single parent creates a linear
history; multiple parents create a merge point.

Snapshots are stored as immutable CAS objects. The content-addressed
hash of the snapshot (after canonical JSON serialization and SHA-256)
is its identity.

## Head Refs

Each project has a head ref under `refs/projects/<principal>/<project>/head`
that points to the current snapshot hash. The head ref is the mutable
entry point into the snapshot DAG.

When a new snapshot is created, the head ref advances to the new hash.
The old snapshot remains in CAS — it is reachable from the DAG until GC
prunes it.

## Retention Policy

Snapshots are retained according to a `RetentionPolicy` with two
categories:

| Field | Default | Matches |
|---|---|---|
| `manual_pushes` | 10 | Snapshots with `source == "push"` or `source == "manual"` |
| `auto_snapshots` | 30 | Snapshots with `source == "fold_back"` or other auto sources |

The head snapshot is **always** kept regardless of policy.

Retention is applied per-project by sorting all reachable snapshots by
`created_at` descending (newest first) and counting per category up to
the policy limit. Everything beyond the limit is marked for removal.

## Topological Compaction

When snapshots are pruned, their children's `parent_hashes` must be
rewritten to point to surviving ancestors. This requires a specific
ordering: parents must be rewritten **before** children, so that children
can reference the parents' final (post-remap) hashes.

The compactor uses **Kahn's algorithm** for topological sort of the
kept snapshots:

1. Build adjacency lists considering only kept nodes
2. Seed the queue with kept nodes that have no kept parents (DAG roots)
3. Process in topological order, remapping `parent_hashes` through
   removed snapshots to surviving ancestors
4. If the result has fewer nodes than the kept set, bail with
   "possible cycle in snapshot DAG"

### Parent Resolution Through Removed Nodes

When a kept snapshot's parent was removed, the compactor resolves
through the removed node's own parents recursively. This handles chains
of removed nodes: if A → B → C and both A and B are removed, C becomes
the surviving ancestor of any child that referenced A or B.

Each rewrite changes the snapshot's content, producing a new CAS object
with a new hash. The compactor maintains a `hash_remap` map (old hash →
new hash) so that children can be updated correctly.

After all kept snapshots are rewritten, if the head snapshot's hash
changed (due to parent rewrite), the head ref is advanced to the new
hash. This is signed with the node key.

## Two-Phase GC Pipeline

Compact runs before sweep because compaction orphans removed snapshots
by rewriting their children's parent references. Sweep then collects
those newly-unreachable objects.

1. **Compact** (opt-in, requires signer): per-project retention-based
   pruning with topological rewrite and head ref advancement
2. **Sweep** (always, no signer needed): mark-and-sweep across all
   sharded directories, deletes unreachable objects and blobs

See [maintenance-gc](../services/maintenance-gc.md) for the full GC
pipeline details.

## DAG vs Linear

Most versioning systems use linear history within a branch (git commits
in a single branch) and DAG across branches. Rye OS uses DAG by default
because:

- Multiple nodes can push to the same project concurrently
- Fold-back operations (post-execution workspace writes) create
  snapshots that reference the pre-push snapshot
- Remote execute creates a push → execute → pull cycle where the pull
  result references the push snapshot

The DAG model is the correct default for a distributed system with
multiple write sources.
