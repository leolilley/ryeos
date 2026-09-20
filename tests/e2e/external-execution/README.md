# External candidate execution qualification

Status: source implementation in progress; **not a supported execution backend**.
No external profile is enabled, no allocator is connected, and these tests must
not be used as evidence that a remote worker can safely run.

## Allocation journal

`test_allocation_journal.py` executes the exact production SQLite DDL extracted
from `ryeos-app`, with no Rust build, node, credentials or cloud contact:

```sh
python3 -B -m unittest discover -s tests/e2e/external-execution -p 'test_*.py'
```

It covers atomic guard updates, one-winner contact claiming, reopen after contact,
immutable reservation identity, quarantine, and refusal of local credential,
session or workspace cleanup while external execution remains unsettled.

The Rust API additionally validates exact dedicated-session/capsule/workspace
ownership, canonical bounded records, contact deadlines, capacity across binding
generations, unique occurrence binding and CAS retention roots. The initial
12-test Rust allocation/channel group passed after Cargo was authorized.
Runtime schema epoch 41 is provisional until
integration with the then-current `next`; it must not collide with another cut.
Epoch 40 first introduced the stable external-obligation reset guard; epoch 41
adds shared-journal initial-state constraints, so old stores are not silently
reinterpreted under a changed SQLite contract.
Schema mismatch is checked before decoding version-specific journal rows. The
independent stable reset guard refuses destructive history reset even when the
controller's own host lifetime has ended.

The only settlement implemented so far is cancellation **before** external
contact. A contacted allocation cannot be released through elapsed TTL, local
worker death, a caller-supplied success flag, or a missing provider response.
The qualified external terminal-observation path is not implemented. Do not wire
this partial journal into live allocation until that path exists.

## Authenticated channel and application journal

`ryeos-state::external_execution` owns strict occurrence-bound public bindings
and domain-separated Ed25519 frames. Owner and supervisor keys are distinct;
neither becomes an enrolled node principal. Frames bind the allocation request,
capsule, base, supervisor runtime, fresh channel nonce, role, sequence, previous
digest and peer acknowledgement. Lifetime, individual frame/chunk sizes, message
count and total journal bytes are bounded. Noncanonical bytes, changed keys,
unknown fields and role-inverted controls are refused.
Ordinary traffic cannot consume the separate one-frame, 4 KiB terminal reserve
in either direction. Only Cancel/Stopped use it; acknowledgements do not.

The runtime journal records both directional sequences and a total arrival
order. Reopen verifies the retained signatures, ordering and lifecycle
projection. Exact duplicates do not grant a new application. Application uses
pending -> claimed -> applied; a crash after claim remains uncertain. Cancel
and stop revoke only provably unclaimed execution input and may overtake an
uncertain application. They do not convert it into no-contact or release its
external allocation. Control observations remain distinct from tool bytes.
Recording quiescence does not revoke preceding input: it must drain before
quiesce application. Export application requires that exact quiesce to have
been applied. A sealed export cannot finish application before its validated
content has durable retention roots.

An owner-signed Cancel also has a narrow sticky-revocation path. It commits an
immutable occurrence-bound record independently of contiguous transcript
reconciliation. Missing predecessors, forks, restart and later data backfill
cannot undo it. Pending and subsequently backfilled input is revoked; claimed
input stays uncertain. This path accepts no ordinary command and proves no
cleanup. Its Rust regression covers cancellation before a missing protocol
frame, reopen and non-executable catch-up. Actual supervisor-side persistence
and dispatch must honor this gate before network replay is enabled.

These are source-level protocol/journal implementations. They are **not** a
running network transport, protected admission binding, guest reconnect loop,
or installed qualification. No HTTP route, general daemon grant or enrolled
guest identity has been enabled.

## Native terminal writer exclusion

Held Lillux preparation retains a pidfd for the native child whose readiness
handshake proved it was namespace PID 1. Ordinary native launches do not gain a
new pidfd capability requirement. `terminate_namespace_for_export` kills that exact init,
waits for exact terminal readiness and exclusively reaps it before returning a
non-serializable proof. This is terminal namespace destruction, not live freeze,
cloud occurrence death, restart recovery or a new process-scope backend.

The trusted supervisor must exclusively own the candidate's writable mounts;
the proof does not exclude writers which were never inside this namespace.
Timeout does not yield a proof. The existing process-handle Drop cleanup can
still block while waiting for kernel exit; an outer supervisor/provider cleanup
owner is required before advertising end-to-end bounded cancellation.

Authored native qualification covers realized and sealed executables plus the
nested-sandbox mode: a descendant continuously writes the candidate, leaves the
process group and enters a nested user/PID namespace when permitted, and must
stop before export. Reusing the consumed
termination handle is refused. Held preparation also covers no execution before
release, single-use release, invalid-deadline refusal and cancellation before
release. A valid, exhausted observation deadline retains the exact handle for
subsequent cleanup. An actual stalled kernel termination still needs a dedicated
qualification case. All three focused native terminal-export tests now pass,
including their realized/sealed and nested-sandbox variants.

## Guest candidate and content import

The executor's `external_candidate` component creates new private candidate
inodes from the exact base CAS closure beneath a protected parent. It does not
make shared or hardlinked cache content writable. Runtime mounts are read-only
and outside `/workspace`; extra channels, devices, writable mounts and host
networking are refused. Lillux prepares the target before exec. Only the exact
authenticated owner release opens that boundary.
Piped preparation creates dedicated candidate stdin/stdout/stderr, with
nonblocking protected relay ends. Native child setup installs these exact ends,
never supervisor bootstrap stdio. Internal release/readiness descriptors reserve
0–2 even when ambient stdio starts closed; the sealed native test covers this.
The candidate owner retains stdin privately; only output readers escape.
Its mutable control methods serialize release, bounded nonblocking partial
input writes, cancellation and capture. Each input has one started sequence
and exact offset; neither a blocked write nor retransmission starts it again.
Cancellation closes stdin and discards unsent bytes before native termination.
Quiesce refuses outstanding partial input. A failed release likewise closes
input instead of permitting subsequent command delivery. These in-process
guards do not replace the supervisor's durable application and revocation
journals or authorize reconstruction of an uncertain execution.
Complete input progress means only that bytes reached the pipe—not endpoint
processing, command completion or a candidate fence. Object-level native tests
for failed release, cancellation during partial writes and capture refusal with
pending input remain qualification gates beyond the input-state unit tests.

This component belongs in a dedicated trusted launcher process: native launch
changes the caller's namespaces. It is not an async-daemon launch function.
The outbound transport/control supervisor and executable bootstrap are still
required; no in-process deadline query substitutes for their finite control loop
or for the platform's independent outer lifetime bound.

After quiescence, capture requires the native namespace terminal proof. It uses
the existing project ingestion, immutable base policy and canonical project
snapshot/file/tree objects. The new content assembler accepts bounded canonical
object/blob chunks, checks offsets and hashes, disallows non-project authority
objects, requires the exact base and policy, and refuses members outside the
completed candidate/evidence closure. It fails permanently on a transfer error.
The caller must retain completed roots before releasing its CAS mutation guard.
The private-constructed imported-content value is revalidated against the
receiving CAS, including bounded streaming hash verification of reused blobs,
then retained by `StateStore::retain_external_candidate_import` under its write
permit/guard. The full B/C check occurs outside the global state lock and SQLite
write transaction. Its privately constructed, guard-borrowing proof permits a
short commit that rechecks the exact channel and export coordinates; it cannot
authorize roots in another receiving store. Immutable runtime rows bind the exact authenticated
export frame. GC adds candidate object roots and observation blob roots
separately, including through quarantine. Merely recording a signed export claim
does not add any candidate root. The authored state test exercises closure/blob
survival across guard release, actual sweep and reopen; the full StateStore
completion/GC race remains an installed integration gate.

Capture checks monotonic time during traversal, per-file streaming, collision
verification and before returning a result. It enforces an aggregate byte bound.
These checks are cooperative: kernel I/O stalls and existing blocking Drop
cleanup still need the independent supervisor/provider lifetime bound. Base
materialization checks between files, not inside every kernel copy operation.

An imported closure is **content evidence only**. Supervisor writer-exclusion
testimony is bound and validated, but that does not independently qualify the
supervisor, settle cloud cleanup, complete the worker, evaluate or publish C.
Durable transfer recovery and the existing candidate completion owner remain
integration gates; no fresh assembler may blindly replay an uncertain frame.

Cargo is now authorized, with one build job and the feature worktree's own
target directory. These commands do not install anything. On space-constrained
hosts add `--config profile.dev.debug=0 --config profile.test.debug=0
--config build.incremental=false` to each command (keep those settings consistent
between packages to avoid redundant builds):

```sh
cargo test --locked --jobs 1 --target-dir target -p lillux --lib terminal_export
cargo test --locked --jobs 1 --target-dir target -p lillux --lib \
  terminal_export -- --include-ignored --test-threads=1
cargo test --locked --jobs 1 --target-dir target -p ryeos-app --lib \
  runtime_db::external_execution -- --test-threads=1
cargo test --locked --jobs 1 --target-dir target -p ryeos-state --lib external_execution
cargo test --locked --jobs 1 --target-dir target -p ryeos-state --lib persistent_session_capsule
cargo test --locked --jobs 1 --target-dir target -p ryeos-engine --lib structured_session_profile
cargo test --locked --jobs 1 --target-dir target -p ryeos-executor --lib execution::ingest
cargo test --locked --jobs 1 --target-dir target -p ryeos-executor --lib execution::external_candidate
cargo test --locked --jobs 1 --target-dir target -p ryeos-executor --lib execution::persistent_session
cargo test --locked --jobs 1 --target-dir target -p ryeos-structured-session --bin ryeos-structured-session-bridge
cargo test --locked --jobs 1 --target-dir target -p ryeos-api --test hosted_command_terminal_replay
```

The native test requires the supported Linux namespace/seccomp/mount floor.
Namespace refusal is a failed qualification gate, not permission to skip
isolation or substitute the diagnostic bwrap fixture.

## Still required

- Complete Codex tool/configuration closure, beyond the pinned routing probe.
- Protected admission binding and actual authenticated outbound transport.
- Guest supervisor executable/control loop and exact runtime/bootstrap realization.
- Wiring and qualification of native capture and candidate-only content transfer.
- Verified external termination, journal settlement, restart reconciliation.
- Compound completion/capture admission through existing candidate owners.
- Thin provider lifecycle adapter, signed profile and coherent bundle refresh.
- Real isolated B -> C -> evaluation acceptance and failure/recovery matrix.

Python/SQLite tests and rustfmt parsing do not qualify any of those gates.

## Review and integration ownership

Independent architecture, security/recovery, and tests/documentation reviews
were run twice. Concrete source corrections cover queued-input/quiesce ordering,
terminal control capacity, imported-content GC ownership, opt-in pidfd acquisition,
candidate-only stdio including closed ambient descriptors, and cooperative
capture budgets. Re-review found no further blocker in that correction set;
this is not approval of the unimplemented runtime or Rust/native qualification.

The next admission change belongs in the existing persistent-session capsule
and profile compiler. The protected supervisor remains outside the dedicated
native launcher's namespaces. Bootstrap uses exact retained products and existing
descriptor authority; keys are separate from candidate content. A guest channel
must use the signed route registry with occurrence-only authentication, not an
ad hoc HTTP route, node enrollment, or ordinary workload-client grant.

`dedicated_session_service::terminate_session_with_bounded_outcome` remains the
completion owner. Before `freezing`, it must join the existing exact command
fence with applied quiescence, retained import, qualified writer exclusion and
independently verified external cleanup. Do not bypass the current journal
guards to enable a trial run. Evaluation/integration/publication stay with their
existing owners. No worker profile is enabled by this source checkpoint.

Pre-Cargo local source checks (2026-09-19): 143 authoring-environment, 21 allocation/
channel SQL, 31 Codex bundle and 5 OpenCode bundle tests passed (200 total).
`git diff --check` and standalone Rust syntax parsing passed. No Cargo commands,
node lifecycle operations, installs or external allocations were performed.

## Cargo qualification in progress (2026-09-19)

One-job, offline builds use this feature worktree's `target`, with dev/test
debug information and incremental compilation disabled. Initial results:

- State framing/export: 5 passed, including actual CAS sweep/reopen.
- Persistent-session capsule: 9 passed from the same compiled state harness.
- Application allocation/channel journal: 12 passed.
- Native terminal export: 3 passed with ignored native tests explicitly enabled,
  including cancellation before release and descendant writer exclusion.
- External SQLite schema regressions: 21 passed on independent review.
- Broader Lillux harness: 234 passed, 18 ignored, one stale descriptor-error
  assertion failed outside the tool sandbox. The assertion is corrected in
  source (stdout/stderr remain refused); its rebuilt rerun is pending. The
  tool sandbox additionally refused three Unix-socket operations; all three
  passed in the exact unsandboxed harness rerun, with no runtime weakening.

Subsequent reviewed corrections add reused-blob corruption refusal, move full
verification outside the state lock, and repair the nested-writer fixture to
fork a single-threaded helper before user namespace creation. A valid 1 ns
native termination deadline now exercises pre-poll expiry and exact-handle
retention/retry; it does not prove behavior under a stalled kernel exit. Native
execution also exposed polling through a source pathname masked by setup's
private root. The fixture now observes the exact pinned directory; the live
capture owner likewise retains descriptor authority, without reopening a hidden
diagnostic path. The native group passed after these corrections. State/app
integrity corrections and executor capture still require affected Cargo groups
to be rerun. No live execution
backend, installation, cloud allocation or model contact is qualified here.

The affected state/app rerun was interrupted explicitly (exit 130) when the
home filesystem fell below 1 GiB free. It is not a test failure or passing
evidence. This thread's single-job target occupied approximately 1 GiB; other
artifacts were not deleted. Moving only this worktree's target to a separate
filesystem is awaiting the operator's choice. Existing binaries remain usable
for non-building diagnostics. No Cargo process from this run remains active.

## Next integration boundaries (reviewed, not implemented)

Retain a pre-allocation command/workspace contract in `PersistentSessionAuthority`
and the persistent-session capsule. Do not embed `ExecutionChannelBinding`: it
already names the capsule, which would create a content-address cycle. Admission
derives exact runtime/product, connector and policy requirements before session
birth; occurrence registration checks them afterward. Provider PID, profile lock
and local cleanup stay with the existing dedicated-session owner.

The guest needs a protected single-binding channel journal, not a miniature
RuntimeDb or enrolled node. Share transcript/application/revocation rules and
transaction-level storage mechanics in `ryeos-state`. Node wrappers retain
session/allocation ownership and import/cleanup responsibilities. Guest restart
without the original live launcher cannot recreate it or replay claimed input;
reconnection to that same live owner is different.

Reuse Lillux inherited duplex/deadline and descriptor-transfer primitives between
supervisor and dedicated launcher. A thin generic supervisor executable belongs
under `crates/tools`, with execution mechanics in executor; the isolation adapter
must not acquire session, CAS or cloud lifecycle ownership. The network supervisor
retains occurrence keys outside candidate namespaces.

Use the signed route registry and an occurrence-only verifier. Existing response
modes provide JSON/SSE, not a duplex WebSocket upgrade. Compose bounded ingress
with event-stream egress and durable acknowledgements, or add a generic registered
duplex mode. Neither permits an ad hoc route, general guest grant or command
backlog replay ahead of sticky cancellation. These source-review decisions are
not transport or installed-activation evidence.

### Shared transcript extraction

`ryeos-state::external_execution::transcript` now owns the exact phase projection,
directional sequence/predecessor/acknowledgement checks and urgent-control
classification. Both append and reopen in the node journal use these rules.
Persisted spellings, wire records and SQLite schema are unchanged; these values
grant no dispatch, cleanup or session authority. Independent architecture and
security source reviews found no semantic regression in the extraction. The
21 SQLite regressions and Rust formatting checks pass; the three new Rust
projection/frontier tests await the affected Cargo rerun after space is available.
This is not yet the shared transaction-level guest journal or supervisor loop.

### Controller runtime-configuration gate

Pinned Codex source review and credential-free binary diagnostics now distinguish
ordinary configuration from managed requirements. Empty ordinary MCP tables
merge with existing entries, so immutable argv alone cannot express deny-all.
The actual managed system file suppresses a configured subprocess canary in the
isolated fixture. See the authoring-environment ledger for the exact scope and
the corrected six-tool and optional selected-skill routing evidence.

The source configuration implementation is generic, not a Codex path in executor:
profile schema 8 and capsule schema 13 require an admitted, bounded source-file
inventory with canonical absolute **namespace** file destinations. It uses exact
capsule identities, pinned and sealed descriptors, enforced read-only isolation,
bridge verification before workload start, and collision refusal against
workspace, profile/state, executable/source/runtime mounts, network inputs,
writable views, `/proc`, `/dev`, `/sys`, `/tmp` and protected control paths.
Private `/tmp` is refused because a read-only file beneath a writable parent
could otherwise be replaced by renaming that parent. Source tests cover bounds,
missing sources, retained-contract drift, disabled isolation, sealed bytes,
executable-authority refusal, mount planning, and read-only bridge verification.
Rebuilt native Lillux execution now verifies the sealed absolute configuration,
its read-only ancestors, and write/unlink/parent-rename refusal across realized,
sealed and nested-sandbox executables, including ordinary descendant exec.
The bridge independently uses that existing namespace-path verifier before
provider startup. This is native mechanism evidence, not installed bridge or
complete external-worker qualification.
Existing flat profile-home auxiliary files keep their semantics. The new scope
must not authorize executable lookup or writes to the host's `/etc`. Node-policy
network runtime files are not the owner of signed provider configuration.

Higher-priority cloud-managed requirements can still change the effective MCP
allowlist. Qualification must resolve/control those inputs before any relevant
startup contact; a later status check is insufficient. Provider contact needed
to discover policy cannot be described as zero provider contact. No such
effective-policy admission is implemented by the diagnostic. Runtime configuration
delivery is implemented separately in source, with empty inventories in existing
profiles; no external worker profile is enabled.

### Configuration qualification checkpoint

Bounded offline Cargo builds in the feature target now establish:

- `cargo check --tests` for engine, executor and structured-session passes.
- State capsule tests: 10 passed; external framing/export/transcript: 8 passed.
- Engine structured-profile tests: 14 passed; isolation tests: 41 passed.
- Lillux ordinary suite: 235 passed, 18 ignored.
- Explicit native terminal-export group: 3 passed.
- Explicit native realized/sealed/nested exec configuration test: 1 passed.
- Focused Python/SQLite and bundle checks: 209 passed (151 + 21 + 32 + 5).

The native exec run exposed a fixture collision: two descendant commands shared
a fixed private `/tmp` proc mountpoint. Each now uses a unique disposable
mountpoint; namespace and readonly checks are unchanged. Rebuilt export tests
also exposed foreign-store initialization beneath an active CAS guard. The
fixture now initializes that store before acquiring the import guard; production
guard rules and cross-store retention refusal are unchanged. Reused-blob
corruption refusal, actual CAS sweep/reopen and shared transcript tests pass.

The source closures for all four Codex workers and the OpenCode worker were
recomputed, and changed signed items were re-signed with the existing development
publisher. This is not a full binary/bundle-manifest refresh or installed
admission. The application allocation/channel group subsequently passed all 14
tests; composed runtime acceptance remains outstanding. No installation, node
lifecycle, cloud or model contact occurred.

Architecture, security and testing reviewers rechecked this configuration slice.
Their concrete `/tmp` pathname-rebinding and fixture-construction findings are
corrected. Runtime inventories remain empty until composed provider-policy
qualification; these reviews do not enable external workers.

### Application-owner precondition before shared journal extraction

Node-side claim and finish now check the exact dedicated-session/profile/workspace
owner inside the same transaction, before even idempotent early returns. Existing
SQL triggers already prevent supported owner replacement; this closes a local
validation asymmetry, not a demonstrated normal-path ownership bypass. The
regression first proves ordinary replacement is refused, then removes only that
guard in a disposable fixture to inject corruption and prove pending/claimed/
applied rows cannot advance under a changed owner.

A separate regression proves an already-claimed delivery can still finish after
revocation/quarantine. Pending input stays revoked and the allocation remains
unsettled. Finish does not require a live execution window or `bound` allocation:
historical delivery acknowledgement is not new dispatch or cleanup proof.
Architecture and security reviews accepted this distinction. All 14 rebuilt
application journal tests and 21 independent SQLite regressions pass. Keep this
node ownership check outside the future shared guest transcript implementation.

The subsequent executor persistent-session test build was explicitly interrupted
(exit 130) before exhausting the filesystem, at approximately 164 MiB free.
That test group has not run; prior successful `cargo check --tests` does not
replace it. No Cargo process from this attempt remains active. Only the feature
target has been populated (approximately 1.6 GiB); relocating that generated
directory to `/tmp` while retaining the worktree-local target path awaits the
operator's choice. No other worktree or installation was modified.

### Reviewed shared journal extraction contract

The next complete slice is a transaction-borrowing adapter in
`ryeos_state::external_execution::journal`, retaining the existing transcript
reducers. It owns common channel/frame/revocation tables, immutable transitions,
binding lookup, signatures, sequence/budget projection, sticky cancellation,
claim/finish and reopen validation. It never commits the caller's transaction.

Owner hooks are mandatory and have no defaults: exact owner validation,
authorization of new application, and exact export-retention qualification.
They cannot disable shared signature, sequence, deadline or revocation checks.
Node hooks retain session/profile/workspace, allocation/quarantine and imported
CAS-root ownership in `ryeos-app`; allocation-dependent delete/reset guards stay
there too. The guest owns a fresh, protected single-binding bootstrap journal,
not node session/allocation tables. Its export gate requires its own durable
native-export retention record, never a fabricated node import row or a skip
flag. Reopen validates exact retained authority but cannot recreate/release a
launcher; without the original process-local owner it remains recovery-only.

Cancellation must still commit before a potentially failing contiguous append.
Claim/owner validation and finish/export retention remain atomic. Actual writes
must share the supervisor's serialized revocation gate; a database claim is not
continuing execution authority. Preserve the 14 node regressions through this
extraction, then add guest fresh/reopen, uncertain-claim, cancellation-gap and
export-retention cases. This is reviewed implementation direction, not an
implemented guest journal or network supervisor.

### Continuation checkpoint — 2026-09-20

The ordered continuation handoff is retained locally at
`.tmp/external-candidate-continuation-20260920.md` in the feature worktree.
It records the six-file uncommitted journal extraction, invariant/owner boundaries
and the remaining composed-runtime gates. The corrected extraction now passes
21 state external-execution tests (13 independent shared-journal tests), all 14
node allocation/channel regressions and 22 exact-DDL SQLite checks. Independent
architecture, security/recovery and tests/schema re-reviews found no remaining
blocker in this extraction after terminal-close recovery was corrected.

The shared core binds every selected row back to authenticated digest/direction/
sequence, requires exact retained export coordinates, refuses advanced initial
rows, and reconstructs terminal-close, quiescence and lifecycle invariants on
reopen. Runtime epoch 41 records the trigger change; the external obligation
reset guard still begins at epoch 40. No supervisor, transport or complete remote
worker is enabled by this evidence.

The prior disk blocker is resolved: the latest filesystem read showed 62 GiB
available on the project filesystem. Cargo remains permitted with one job and
the feature-local target; no target relocation, installation or lifecycle action
was performed. Recheck available space before resuming builds.
