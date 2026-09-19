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
generations, unique occurrence binding and CAS retention roots. Its tests still
require an operator-run build. Runtime schema epoch 40 is provisional until
integration with the then-current `next`; it must not collide with another cut.
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
release. An actual unresolved kernel-termination timeout still needs a dedicated
qualification case. These tests are
**not run yet**.

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
receiving CAS, then retained by `StateStore::retain_external_candidate_import`
under its write permit/guard. Immutable runtime rows bind the exact authenticated
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

Operator-only commands (not installation; use the feature worktree, never the
primary checkout's target directory):

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

Latest local source checks (2026-09-19): 143 authoring-environment, 20 allocation/
channel SQL, 31 Codex bundle and 5 OpenCode bundle tests passed (199 total).
`git diff --check` and standalone Rust syntax parsing passed. No Cargo commands,
node lifecycle operations, installs or external allocations were performed.
