<!-- ryeos:signed:2026-09-01T00:15:48Z:41bd90d54845b0a4ec32d948a538942e5daa2d2923ffc462b8f1b0e5047b26ca:1XFSbPpjTlP9o1ePtb3OQMP5Ji9IA88CkWeC4SHih02c1J3StFlOhhWhLjz7exhy5zK6RmAW/OKpBRW/a4WtDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "persistence-schema-evolution"
title: "Persistence Schema Evolution"
description: "Rules for immutable CAS wire identities, retained SQLite migrations, rebuildable projections, and explicit history retirement"
entry_type: reference
version: "1.9.0"
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
- admitted launch capsule schema 16;
- runtime launch metadata epoch 21;
- the standalone runtime project-authority envelope epoch 3; and
- the owned runtime SQLite operator schema epoch 19 (encoded in the RyeOS
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

Runtime epoch 19 retains the epoch-8 hosted-worker substrate,
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
state. Epoch 17 admits persistent-session capsule schema 5 and makes the
runtime descriptor's signed content-dependency ceiling explicit. An epoch-16
descriptor omits that authority and is never normalized to the current empty
policy during recovery. Epoch 18 cleanly replaces the detached-only spawn
intent with one generic runtime-action intent. It binds each runtime-asserted
opaque operation ID to the authoritative chain, first caller, action mode,
exact daemon-derived request hash, and one daemon-minted child identity before
contact. Detached project/launch authority remains a mode-constrained extension
of that same record; inline actions cannot populate it. Epoch 19 admits launch
metadata epoch 21 and admitted launch capsule schema 16. Their sealed root
request retains the exact optional ingress-authenticated handler context, and
callbacks bind that caller/site authority instead of reconstructing transport
authentication from a principal string. Machine continuations cannot replace
the principal, operator continuations rebind it only from a fresh authenticated
handler, and remote placement clears source-node handler authority. There is no
epoch-18 reader, handler-context reconstruction fallback, alternate inline
ledger, operation-ID compatibility alias, or migration. No execution-history
reader or migration for epochs 1 through 18 remains. An explicit reset
classifies ownership and ordering solely from the outer runtime application-ID
family and epoch. Once the store is proven to be an intact, strictly older
RyeOS RuntimeDb, every predecessor table, index, view, trigger, row, and
embedded authority remains opaque and the complete schema is discarded. Reset
must not compare a predecessor layout with the current table set or grow a
historical schema allowlist.

OperationalDb is the only credential-profile authority carried through the
cutover. Reset validates and captures its exact current records before
publishing destructive intent, monotonically invalidates any `enrolling`
ceremony whose session history is being retired, creates an empty exact-current
RuntimeDb, and rebuilds the runtime projection from those stable records. It
never extracts or merges a predecessor RuntimeDb credential table. A runtime
credential transition not durably acknowledged in OperationalDb is not
preservation authority across an explicitly confirmed whole-history cutover.
Earlier history requires the explicit retirement ceremony below; normal
startup never rewrites it.

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

Replay indexes inside that stable database have their own clean-cut epoch,
currently epoch 6.
They are not authority-compatible merely because the surrounding SQLite schema
is current: a dispatch-effect record retains its complete admitted execution
closure, including the exact admitted-launch-capsule schema. When that closure
contract changes, ordinary open refuses the predecessor replay epoch and names
the explicit offline activation command:

```bash
ryeos node reset replay-indexes --confirm
```

That operation retires only predecessor `dispatch.effect` rows. It preserves
current provider-call evidence, credential profiles, sync state, admission
attestations, accounting state, signed heads, and CAS bytes. The next ordinary
GC reclaims objects that are no longer rooted. Launch-capsule schema changes
must therefore make an explicit replay-epoch decision; they must never leave
predecessor effect rows silently pinning an undecodable closure.

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

The confirmed reset accepts any intact, strictly older database in the owned
runtime application-ID family. Unknown predecessor schema objects are expected
at this boundary; they block ordinary open but never block the explicit
retirement operation. Unowned databases, a newer runtime epoch, or corruption
still fail closed without mutation. This invariant is tested with deliberately
unknown schema objects rather than fixtures for individual historical epochs.

Normal initialization and package installation never invoke this destructive
transaction implicitly. After installing a release with a new clean-cut
execution contract, daemon startup reports this exact command when retained
history belongs to the previous contract. Run the dry-run, make the explicit
retirement decision, apply the confirmed command, and then start the already
installed daemon. Do not add predecessor readers or rewrite immutable CAS
objects merely to make startup accept the old epoch.
