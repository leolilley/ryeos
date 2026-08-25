<!-- ryeos:signed:2026-08-25T02:40:34Z:fe019e5dfd4281c444eba1e6dbe77db664df7a19b1dbf389ae78d092be1a5b11:rbhvTopfqaf4fQoNyZPIrHleTR2yz+tyhRw2dFgFCiNUPo6k2oz85IQzmRpwBuz0gUCTEt3WWtj4wZdcj3XWCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "persistence-schema-evolution"
title: "Persistence Schema Evolution"
description: "Rules for immutable CAS wire identities, retained SQLite migrations, rebuildable projections, and explicit history retirement"
entry_type: reference
version: "1.4.0"
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

- thread snapshot schema 10;
- project snapshot schema 5;
- admitted launch capsule schema 14;
- runtime launch metadata epoch 18;
- the standalone runtime project-authority envelope epoch 3; and
- the owned runtime SQLite operator schema epoch 10 (encoded in the RyeOS
  `PRAGMA application_id` family).

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

Runtime epoch 10 includes the epoch-8 session-bound worker,
credential-generation fencing, command/approval contact ledgers, observation
frontier with a cross-epoch cumulative event ceiling, candidate-disposition,
and multi-epoch process-history contracts, the epoch-9 exact
retained-current-HEAD destination, and the canonical pre-contact payload for
every unsettled accepted worker observation batch. No
epoch-1-through-9 reader or migration remains. Earlier history requires the
explicit retirement ceremony below; normal startup never rewrites it.

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
# Inspect the available retirement scope first.
ryeos node gc \
  --discard-thread-history \
  --discard-project-heads \
  --dry-run

# Apply the explicitly confirmed clean cutover.
ryeos node gc \
  --discard-thread-history \
  --discard-project-heads \
  --confirm-discard-thread-history \
  --confirm-discard-project-heads
```

This is separate from normal retention and GC, requires the daemon to be
stopped, and coordinates every thread-derived store under one durable recovery
marker so interruption can be resumed. The transaction publishes that discard
intent first, resets an incompatible runtime schema when required, retires
chain and project HEADs, clears the remaining runtime execution state and
scheduler fire history/projections, and only then completes the marker.
Physical CAS sweeping may happen in the same command or later. It does not
remove project worktrees, bundles, vault values, operator/node identities, or
signing keys.

For an incompatible runtime schema, dry-run reports its runtime-row count as
unavailable rather than decoding rows or presenting a false zero. Authoritative
chain/project HEADs and filesystem artifact counts remain inspectable before
confirmation. The confirmed report also preserves that unavailable
classification rather than claiming that the empty replacement schema's zero
rows were the retired-row count. Dry-run acquires only the existing operator
lock without rewriting it, copies the descriptor-pinned SQLite database and
sidecars into a disposable inspection directory for runtime accounting, does
the same for the scheduler database and sidecars, and opens SQLite only on
those copies; the source runtime namespace is never a SQLite write target.

Normal initialization and package installation never invoke this destructive
transaction implicitly. After installing a release with a new clean-cut
execution contract, daemon startup reports this exact command when retained
history belongs to the previous contract. Run the dry-run, make the explicit
retirement decision, apply the confirmed command, and then start the already
installed daemon. Do not add predecessor readers or rewrite immutable CAS
objects merely to make startup accept the old epoch.
