<!-- ryeos:signed:2026-09-12T01:05:15Z:784f07e2ea783f639bbe4f6b769b01452bf1ea47f5aa16c02402337cf910ea35:lRUwBwS4MPpmJ3P8QGtf3yD1SejRdwTMMLIMaZ4EfP2Ey+moG20sYZS/02dkUHfyQubJZC9BNv4I36eWYRlUCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "persistence-schema-evolution"
title: "Persistence Schema Evolution"
description: "Rules for immutable CAS wire identities, retained SQLite migrations, rebuildable projections, and explicit history retirement"
entry_type: reference
version: "2.3.1"
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

- sealed root execution request schema 20;
- thread snapshot schema 13;
- project snapshot schema 5;
- admitted launch capsule schema 28;
- persistent-session capsule schema 11 (enforced session source is runtime-owned,
  never a project mountpoint; predecessor capsules are not reinterpreted);
- runtime launch metadata epoch 36;
- the standalone runtime project-authority envelope epoch 5; and
- the owned runtime SQLite operator schema epoch 34 (encoded in the RyeOS
  `PRAGMA application_id` family).

Hosted command-outbox startup replay preserves an exact predecessor session
capsule as opaque history only after authoritative terminal-thread testimony,
a terminal detached session, and the existing placement-worker index prove no
unsettled boot remains. Classification uses the verified capsule's positive
integer outer epoch, never a predecessor decoder. It does not repair those
command rows, assert settlement, or make them replayable. Current malformed,
missing/invalid epoch and future capsules remain errors, as do active or
cleanup-unproved placements. No reset or rewrite of retained history is part
of this terminal-history classification.

The combined workload-invocation and environment-product cut uses launch
metadata 35 and runtime epoch 34. It preserves invocation ingress provenance
and product selection/retention together; neither branch's earlier epoch may be
reinterpreted as the combined authority. The existing
runtime-action row now retains trusted ingress provenance; a partial unique
index fences protocol session/turn/call identity across boot-grant rotation.
Decoding recomputes the operation coordinate from retained source and grant.
The request protocol is v2, environment contract v6, and compiled structured
profile version 3. Structured-session wire version 2 binds command-progress
acknowledgements to the exact request and digest. The signed protocol and bridge
select the same version; old peers fail their wire identity check. Predecessor
authority is not defaulted or reinterpreted.

The combined isolation-adapter wire is v10. It retains both nested-sandbox
process authority and descriptor-relative runtime-view descendants. Earlier
v8/v9 launch messages are not the combined contract; historical protocol
identity remains testimony, not permission to execute an old launch shape.

The isolation policy v4 cut required explicit `network.runtime_files` (including an
explicit empty list). Launch metadata 31 binds the sealed input digests in the
node isolation admission class. Missing predecessor fields are not defaulted;
older launch metadata is classified before nested decoding. This cut does not
change the transient adapter protocol, portable capsule or SQLite table schema.
Installed policy replacement remains explicit and separate from source edits.

The numbers identify independently evolving contracts. A change to a nested
execution authority advances every enclosing durable contract whose bytes
change. RyeOS intentionally carries no predecessor reader for these current
execution formats.

The fixed-parent live-confinement cut advances each enclosing project-authority
contract together. Isolation policy v2 requires an explicit `live_project`
selection and bounds; adapter protocol v5 carries the compiled filtered views.
Runtime epoch 26 and replay-index epoch 9 refuse predecessor execution
authority. No old mask contract is upgraded, and installed policies are not
silently filled in. Qualification requires explicit, scoped policy-generation
and execution/replay retirement before using newly built artifacts. These
source changes do not themselves mutate an installed node.

The subsequent PID-proc choice uses isolation policy v3 and adapter protocol
v6. Launch metadata advances to 29 because its nested isolation provenance
contains the strict adapter-protocol enum; prior rows are classified before
decoding that enum. Compiled isolation plans are transient and are not embedded
in sealed requests or launch capsules, so this change alone does not advance
their epochs or runtime SQLite epoch 26. Installed policy generations require
explicit replacement; no missing-field defaults or predecessor adapters exist.

The combined shared-authoring/content admission cut advances sealed request
18, capsule 25 and runtime epoch 27. Prepared launches now retain intersected
source/runtime limits and independently enforced receiving-kind content
ceilings; recovery cannot substitute an ambient kind definition. Launch
metadata 29 carries both this authority and the proc protocol cut; neither
standalone v29 shape was installed before the combined generation.

The lossless workspace-mutation cut uses isolation-adapter protocol v8 and launch
metadata 30, because the nested protocol identity is strict. Runtime epoch 28
adds exact per-launch workspace/view/launch-owner membership to `thread_runtime`
and retains creator process identity in the existing construction journal.
These are local operational authorities, never portable request fields. Cold
recovery must settle every old incarnation's borrowers before constructing a
new view; a same-daemon retry transfers its original retained view explicitly.
Predecessor metadata remains opaque history and predecessor runtime state
requires the explicit scoped reset. No missing binding is decoded as proof that
an old worker never contacted a workspace. Sealed request 18, capsule 25 and
policy v3 are unchanged by this transient protocol/operational membership cut.

Dedicated-worker admission reserves its worker ID and boot epoch before process
contact, including recovery into the current workspace. An admitted attempt
without an attached process row remains quarantined after restart; neither
missing attachment nor an older reaped boot releases its credential fence.
Only exact failed-start cleanup testimony can settle an unattached attempt.

The process-scope cut uses isolation policy v5, launch metadata v32 and runtime
epoch30. Policy explicitly selects an unconfigured facility or a Lillux-owned
configuration and node control budget. Launch provenance records actually
qualified scope capabilities. Process identity v2 requires explicit nullable
scope recovery evidence: null is the separate strict-group contract, never a
missing-field fallback. Runtime admission retains pre-contact scope authority
in the existing dedicated-session attempt, including worker/boot and daemon
generation fences. WorkerProcessRecord still requires complete attachment
identity. Scope absence after a failed lookup is not proof of cleanup, and
scope emptiness does not substitute for a separately owed wrapper reap. Old
metadata remains opaque history; old runtime state requires explicit reset.

The epoch30 cut also splits pre-contact reservation into planned
allocation and bound resource phases on that existing session row. A Lillux
allocation v2 is journaled before kernel creation; exact Lillux recovery v4 is
bound before spawn. Both retain the original admitted control ceiling; restart
cannot extend it. Lillux configuration v3 selects a stable host location;
allocation and recovery retain its captured boot/directory incarnation. An
ephemeral cgroup inode is not authored into reboot-stable node policy. Required explicit null
recovery means unbound, not an older compatible shape. Recovery may discard a
fenced unbound allocation, but may never bind or replay its creation to launch
a new worker. A bound resource still retains its separate wrapper-reap duty.
Startup may settle that duty using Lillux proof that the recorded host lifetime
ended, without reopening a same-named resource on the new host lifetime. A
same-host empty or unavailable scope continues to retain the independent duty.

Isolation compilation consumes the actual retained scope into its private
attachment-required request. Ordinary launches retain strict-group containment;
configured scope capability alone cannot relax it. The exact scope cannot be
substituted or dropped between compilation and spawn. This cut
uses strict adapter protocol v8: every plan includes `nested_sandbox`, and
`pid_namespace_nested` is a separate explicit proc ceiling. Configured scope
policy must explicitly include its nested permission. Native generation
inspection, actual retained scope, and node permission must all agree; an
ordinary launch cannot acquire the relaxed plan from configuration alone.
There is no missing-field default or v7 compatibility decoder.

Closed-workspace scope retirement is recorded on that existing session row.
Its reserved/retired intent contains the canonical scope set derived from all
settled worker epochs and any settled pre-contact reservation. Kernel removal
runs outside the StateStore lock and can be resumed after a crash. Only this
already-reserved removal operation may treat an absent resource as removed;
passive liveness and process recovery must still refuse same-boot absence.
Ordinary chain retention counts unsettled scope retirement separately from
PID/group liveness, including prior attached epochs after the active attempt's
scope field has been cleared. Terminal status or a reserved-but-incomplete
removal cannot release that pin. Current-schema offline history discard uses
the same obligation query before publishing destructive intent and refuses
until retirement completes; it does not equate daemon exclusion with cleanup.
An independent singleton `execution_lifetime_fence` v1 row is created with the
scope-capable runtime cut. Reservation writes its opaque Lillux host-lifetime
witness in the same transaction as the existing session allocation. The last
exact scope retirement clears it in the same transaction; no separate worker
registry or second scope-control owner exists. Across execution-schema changes,
offline reset reads only this stable bounded contract, never obsolete launch
rows. It requires either a settled/null witness or Lillux proof that the former
host lifetime ended. Daemon restart, a missing PID, an instantaneous empty scope,
or the diagnostic lifecycle marker cannot replace that proof. Missing/malformed
fences in scope-capable stores are refusals, not null defaults. A reservation
after host reboot may advance an ended witness while preserving old per-worker
history. Current-schema reset still requires exact workspace/scope retirement.
The attachment history is retained unchanged, and scopes remain allocated
while a workspace or borrower can still use their live cleanup evidence.

Authoritative readers must inspect the outer object kind and numeric epoch from
generic JSON before deserializing nested typed data. Only after that gate may
they deserialize, validate the complete current shape, and verify canonical
bytes/hash identity. This ordering prevents an old nested authority from
surfacing as an incidental field error or being partially reinterpreted under a
current parent epoch.

## Retained SQLite source-of-truth stores

External-content binding objects use `ryeos.external_content_binding.v3`,
binding-subject v3 and the existing binding-head epoch 4. Pinned consumers
require an explicit nullable source-closure projection: null records a
declarative program with no separately executed source tree, not a failure to
admit required source. Exact generation and pre-realization effective-program
identity remain mandatory. The source admission pass still rejects invalid
source contracts. Existing binding heads require the normal explicit
`ryeos node reset external-content-bindings` cutover and target-local rebinding;
retained content, project heads, identities and credentials are not discarded.
This shape occurs in the binding object, not inline in launch capsules, so it
does not allocate another unrelated launch/history epoch.

Runtime and operational databases retain facts that cannot be reconstructed
solely from signed heads, but they have different retirement policies.

`runtime.sqlite3` accepts only its exact current owned table/index contract and
the exact current envelopes stored in its JSON columns. Normal open never
migrates or normalizes a predecessor. Any mismatch leaves the file untouched
and requires the explicit operator-confirmed thread-history/project-head reset.

Runtime epoch 25 retains the epoch-8 hosted-worker substrate,
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
ledger, operation-ID compatibility alias, or migration. Epoch 20 admits sealed
root execution request schema 12, thread snapshot schema 11, admitted launch
capsule schema 17, and launch metadata epoch 22. Every enclosing durable
contract advances because captured node-history policy provenance changed from
the predecessor tagged `signed_config`/`missing_config` wrapper to the flat
exact signed policy-item identity. No current reader reinterprets the old
nested shape. Epoch 21 admits sealed root execution request schema 13,
admitted launch capsule schema 18, and launch metadata epoch 23. A remotely
adopted invocation now seals the exact current target-node operator grant that
authorized access to target-private project and credential state. That grant
is placement authority and remains excluded from portable exact-program
identity. Epoch 21 also makes the handoff credential reservation the durable
owner of the exact target project-HEAD fence from target preparation through
authoritative adoption. Every online project-HEAD writer, including compact
GC, serializes with that reservation authority; predecessor reservation rows
cannot authorize this contract.
Epoch 22 extends that same runtime-action intent with the signed generic
workspace-access class; exact placement-local worker boot, admitted grant and
project-authority digests; a crash-recoverable phase barrier; and the exact
immutable input generation selected under quiescence. It introduces no second
workspace lease or child ledger: `execution_workspace` remains the physical
workspace journal, while the runtime-action row remains the one operation and
child identity. Predecessor rows cannot authorize this relationship.
Epoch 25 combines that workspace-operation authority with bounded worker
outcomes and their original completion boot epochs, independently completed
candidate evaluation, and journaled qualification/disposition. It admits
sealed request 16, launch metadata 27, admitted capsule 23 and persistent-session
capsule 8. Neither the source-local epoch 22 nor the independently developed
campaign epochs 22–24
can authorize this combined contract. No predecessor execution-history reader
or open-time migration remains.

An explicit reset
classifies ownership and ordering solely from the outer runtime application-ID
family and epoch. Once the store is proven to be an intact, strictly older
RyeOS RuntimeDb, every predecessor table, index, view, trigger, row, and
embedded authority remains opaque and the complete schema is discarded. Reset
must not compare a predecessor layout with the current table set or grow a
historical schema allowlist.

Admitted launch capsule schema 19 adds the generic target-bound process-
environment contribution. Schema 20 distinguishes standalone retained
command bytes from an executable member at its realization-relative location
inside a complete pinned realization tree. Schema 21 requires the serialized
execution plan to carry the signed kind-schema-projected per-execution network
ceiling. Runtime launch metadata epochs 24 and 25 advance with those enclosing
authority changes. Predecessor plans never acquire a realization layout or
`node_policy` networking by omission; their outer epochs are classified before
nested decoding and retained only as opaque history.

Admitted launch capsule schema 22 adds explicit realization mount roots and
the filesystem ceiling beside networking. Schema 23 combines these with
scheduled-fire invocation authority and candidate-purpose/dual-generation
executable authority. Scheduled-fire coordinates remain outside executable
identity; independently evaluated candidate authority remains inside it.
Launch metadata 27 and sealed request 16 carry the combined contract without
reinterpreting either predecessor branch's envelopes.

Persistent-session capsule schema 8 combines required exact evidence-attachment
bindings with schema 7's retained filesystem/network ceilings and realization
mount roots. Even an empty evidence list is explicit; a predecessor program
cannot acquire this authority by omission during recovery.

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
currently epoch 10. Epoch 10 binds dispatch-effect replay to admitted launch
capsule schema 25, including retained receiving-kind content ceilings and
explicit fixed-parent live filesystem authority.
An epoch-9 record cannot prove that authority and
is therefore retired rather than reinterpreted.
They are not authority-compatible merely because the surrounding SQLite schema
is current: a dispatch-effect record retains its complete admitted execution
closure, including the exact admitted-launch-capsule schema. When that closure
contract changes, ordinary open refuses the predecessor replay epoch and names
the explicit offline activation command:

```bash
ryeos node reset replay-indexes --confirm
```

For the exact immediate predecessor, that operation retires only
`dispatch.effect` rows and preserves provider-call evidence because the epoch
transition explicitly proves that namespace remains current. If a node skipped
one or more replay generations, the same explicit reset still succeeds but
retires every replay row; RyeOS does not compose unshipped compatibility claims
across the skipped epochs. Credential profiles, sync state, admission
attestations, accounting state, signed heads, and CAS bytes are preserved in
both cases. The next ordinary GC reclaims objects that are no longer rooted.
Launch-capsule schema changes must therefore make an explicit replay-epoch
decision; they must never leave predecessor effect rows silently pinning an
undecodable closure.

The confirmed `ryeos node reset execution-history --confirm` operation composes
this same replay activation; operators do not need to discover and run a second
reset first. It inspects the replay retirement scope and stable credential
records under the existing stopped-node and pinned namespace locks, including
during dry-run. A restricted preparation handle cannot service replay. Only
after publishing the existing durable history-discard intent does it activate
the replay indexes, invalidate retired enrollment ceremonies, and reset runtime
history. Active credential profiles and private-home identities remain stable.
A retry after interruption reuses that intent and the idempotent activation.
Ordinary startup remains strict; this is not a stale-index compatibility path.
Both CLI reset reports expose the actual source/target epoch and whether no
rows, dispatch-effect rows, or all replay rows were selected for retirement.

`accounting.sqlite3` is the durable financial source of truth paired with its
node-local external financial anchor. Accounting schema v2 extends the exact v1
ledger with launch-gate directive bindings and operation-keyed cross-site
allowance export/import tables. Its sole automatic migration accepts only the
fully validated exact v1 application ID and complete v1 table/index SQL, then
applies the additive v2 contract in one immediate transaction. Every retained
v1 gate is materialized with an explicit null directive binding because v1
committed execution-budget authority only; the migration never infers broader
launch authority from per-attempt directive rows. It then validates the
complete resulting schema before commit. This is a forward migration of
financial authority, not an execution-format compatibility reader.

The v2 export transition atomically records the externally anchored financial
sequence, immutable transfer receipt, and per-account debit rows. Target import
creates zero-use `prepared` accounts, then records the exact rooted source
transfer and activates them atomically during remote adoption. An exported
source allowance is irreversible: recovery completes the associated writer cut
instead of aborting or refunding it. Startup verification recomputes transfer
receipt identity, financial-transition linkage, per-account debit aggregates,
and rejects open predecessor gates that lack the current exact directive
binding. Unknown or malformed accounting layouts still fail closed and are
never normalized.

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
