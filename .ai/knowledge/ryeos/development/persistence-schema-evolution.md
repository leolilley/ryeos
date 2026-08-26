<!-- ryeos:signed:2026-08-26T23:06:47Z:3bd148e72b3a0f75e00b240db8b69122e9d56638a2f5fc4238efb831155c90e6:xtNfsm0qXBgPWgzSqYEorsDmlcYyOqhUg3qrKMFTR1R6CPx8YoOLz3/8D/e0TvIFD6fyLUOY9Wkz3E0tsl1WBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "persistence-schema-evolution"
title: "Persistence Schema Evolution"
description: "Rules for immutable CAS wire identities, retained SQLite migrations, rebuildable projections, and explicit history retirement"
entry_type: reference
version: "1.7.0"
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
- admitted launch capsule schema 15;
- runtime launch metadata epoch 20;
- the standalone runtime project-authority envelope epoch 3; and
- the owned runtime SQLite operator schema epoch 16 (encoded in the RyeOS
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

Runtime epoch 16 includes the epoch-8 hosted-worker substrate,
credential-generation fencing, command/approval contact ledgers, observation
frontier with a cross-epoch cumulative event ceiling, candidate-disposition,
and multi-epoch process-history contracts, the epoch-9 exact
retained-current-HEAD destination, and the canonical pre-contact payload for
every unsettled accepted worker observation batch, plus the generic
project/opaque-backend-state execution-workspace authority and isolation
adapter protocol-v3 journal cut, plus a revisioned live projection of stable
credential-profile lifecycle authority. Epoch 13 cleanly separates stable
`chain_root_id` addressing from exact `placement_thread_id` and worker-boot
fences throughout the hosted-worker projection; it carries no hosted
`session_id` alias. Epoch 14 admits the path-free launch contract introduced by
launch metadata epoch 19 and admitted
launch capsule schema 15. Their outer exact-program identity is path-free,
classifies every sealed invocation field explicitly, and commits the exact
engine-resolved ref-binding records used by managed launch preparation. Epoch
15 adds the durable target credential-profile generation reservation consumed
atomically by an imported successor's dedicated-session admission; restart
keeps an unconsumed reservation fenced instead of confusing it with an
abandoned worker lock. Epoch 16 admits launch metadata epoch 20, whose machine
continuations durably distinguish predecessor-native checkpoint resume from a
cold runtime start after an authoritative higher layer has already restored
state. No execution-history reader or migration for epochs 1 through 15
remains. The
explicit reset may extract only the provider-neutral credential-profile table
from epochs 6 through 11. From epoch 12 onward OperationalDb is the stable
profile authority, but the exact revisioned runtime projection is folded into
it before replacement because a crash between the runtime commit and stable
commit can leave that projection one authority revision ahead. Only the known
credential table shape is decoded; reset never decodes or carries forward an
execution/thread row. A surviving `enrolling` state is invalidated after the
revision fold because its ceremony history is being retired, and that
invalidation advances stable authority monotonically. The empty current
runtime projection is then rebuilt from the validated stable records. Earlier
history requires the explicit retirement ceremony below; normal startup never
rewrites it.

`operational.sqlite3` owns stable credential-profile ownership, lifecycle,
confirmed account evidence, generation, and tombstones in addition to its
other non-reconstructable records. Its explicit atomic v4-to-v5 forward
migration creates that authority table; a monotonic profile revision repairs a
crash gap against RuntimeDb's live session/lease projection. The store must
never be silently reset or archived. Schema v6 retains each remote sync job's
immutable typed operation. Its v5-to-v6 migration proves the exact predecessor
table, gives legacy jobs an explicit non-authoritative recovery envelope, and
atomically rebuilds the table into the same column order and SQL as a fresh v6
store. The exact appended-column intermediate produced by the original v6
migrator is also recognized and repaired; no unknown layout is modified.

## Rebuildable SQLite projections

Thread and scheduler projections are derived views. Their schema can move by
building a new complete current projection from durable authority and atomically
publishing it. Normal startup never guesses that unsupported authoritative CAS
objects are disposable.

## Explicit history retirement

If the operator chooses to discard a whole local execution-history epoch, use:

```bash
# Inspect the available retirement scope first.
ryeos node reset execution-history \
  --include-project-heads \
  --dry-run

# Apply the explicitly confirmed clean cutover.
ryeos node reset execution-history \
  --include-project-heads \
  --confirm \
  --confirm-project-heads
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
