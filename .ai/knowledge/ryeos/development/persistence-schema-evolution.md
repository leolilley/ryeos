<!-- ryeos:signed:2026-07-21T00:24:55Z:3de483ee2e9cd80ad3f98e032b96968e11ee637af91a4fb26c49c0e3428beee0:pTY2Gpbt2Bb0FYNAHGWJbb1KJnJoyR0DxecKDRvsV+EhQhUAlEmU5Q4GagoahzP3dMSB3WsXQ5aQJ7w7x3TLAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "persistence-schema-evolution"
title: "Persistence Schema Evolution"
description: "Rules for immutable CAS wire identities, retained SQLite migrations, rebuildable projections, and explicit history retirement"
entry_type: reference
version: "1.1.0"
```

# Persistence Schema Evolution

RyeOS uses different evolution rules for immutable CAS objects, retained
source-of-truth databases, and rebuildable projections. A shared integer field
does not make these stores interchangeable.

## Immutable CAS wire schemas

An object `kind` plus `schema` identifies one immutable wire shape. Once any
object with that identity can have been published, the number is permanently
occupied. Removing old readers does not make the number reusable.

- A changed wire shape receives a new schema number.
- Clean-cut releases may support only the new number; they still must not reset
  it to `1` or reuse an older number.
- Readers fail closed on unsupported numbers and incomplete current shapes.
- Existing CAS bytes are never rewritten in place because their canonical bytes
  and SHA-256 hash are their identity.

The current clean-cut execution formats include:

- thread snapshot schema 6;
- project snapshot schema 5;
- admitted launch capsule schema 2;
- runtime launch metadata epoch 10; and
- the standalone runtime project-authority envelope epoch 1.

The numbers identify independently evolving contracts. A change to a nested
execution authority advances every enclosing durable contract whose bytes
change. RyeOS intentionally carries no predecessor reader for these current
execution formats.

Authoritative readers must inspect the outer object kind and numeric epoch from
generic JSON before deserializing nested typed data. Only after that gate may
they deserialize, validate the complete current shape, and verify canonical
bytes/hash identity. This ordering prevents an old nested authority from
surfacing as an incidental field error or being partially reinterpreted under a
current parent epoch.

## Retained SQLite source-of-truth stores

Runtime and operational databases retain facts that cannot be reconstructed
solely from signed heads, but they have different retirement policies.

`runtime.sqlite3` accepts only its exact current owned table/index contract and
the exact current envelopes stored in its JSON columns. Normal open never
migrates or normalizes a predecessor. Any mismatch leaves the file untouched
and requires the explicit operator-confirmed thread-history/project-head reset.

`operational.sqlite3` accepts only its exact current schema today. If a deployed
predecessor ever exists, preserving its non-reconstructable facts requires a
separately designed, explicit, atomic forward migration. It must never be
silently reset or archived.

## Rebuildable SQLite projections

Thread and scheduler projections are derived views. Their schema can move by
building a new complete current projection from durable authority and atomically
publishing it. Normal startup never guesses that unsupported authoritative CAS
objects are disposable.

## Explicit history retirement

If the operator chooses to discard a whole local execution-history epoch, use:

```bash
ryeos node gc \
  --discard-thread-history \
  --discard-project-heads \
  --confirm-discard-thread-history \
  --confirm-discard-project-heads
```

This is separate from normal retention and GC, requires the daemon to be
stopped, and coordinates every thread-derived store under one durable recovery
marker so interruption can be resumed. It retires thread roots and project
heads before replacing incompatible runtime state. Physical CAS sweeping may
happen in the same command or later. It does not remove project worktrees,
bundles, vault values, operator/node identities, or signing keys.
