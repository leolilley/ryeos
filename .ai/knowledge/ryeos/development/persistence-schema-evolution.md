```yaml
category: "ryeos/development"
name: "persistence-schema-evolution"
title: "Persistence Schema Evolution"
description: "Rules for immutable CAS wire identities, retained SQLite migrations, rebuildable projections, and explicit history retirement"
entry_type: reference
version: "1.0.0"
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

The current clean-cut formats are thread snapshot schema 2 and project snapshot
schema 4. RyeOS intentionally carries no schema-1 thread-snapshot reader and no
pre-schema-4 project-snapshot reader.

## Retained SQLite source-of-truth stores

Runtime and operational databases retain facts that cannot be reconstructed
solely from signed heads. Recognized deployed predecessor schemas receive an
explicit, atomic, in-place forward migration. Unknown, foreign, or contradictory
shapes fail without being renamed, archived, reset, or partially mutated.

## Rebuildable SQLite projections

Thread and scheduler projections are derived views. Their schema can move by
building a new complete current projection from durable authority and atomically
publishing it. Normal startup never guesses that unsupported authoritative CAS
objects are disposable.

## Explicit history retirement

If the operator chooses to discard a whole local thread-history epoch, use
`ryeos node gc --discard-thread-history`. This is separate from normal retention
and GC, requires the daemon to be stopped, requires explicit confirmation for
mutation, coordinates every thread-derived store under one durable recovery
marker, and can be resumed after interruption. It retires roots first; physical
CAS sweeping may happen in the same command or later.
