<!-- ryeos:signed:2026-08-26T02:45:10Z:6f1a95f5b30e2964b2facb7957b3b92c3a83f164f94832dcdcf1de8695de9adb:fvwNQMBz3R8G1TeLYVVXYqn1GOpIC8VL7PjtXsRKSpXC6Guu3QcZV4m9P1nUZ+g2LWiTnxmabLKceAdXz+uOBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/core/execution"
name: "worker-hosted-execution"
title: "Worker-Hosted Execution"
description: "Implemented authority, protocol, lifecycle, recovery, and publication contracts for session-bound hosted workers"
entry_type: reference
version: "1.0.0"
```

# Worker-Hosted Execution

Worker-hosted execution runs one long-lived subordinate workload for one
ordinary RyeOS root execution. It is generic execution substrate, not an
engine kind, agent identity, or local-inference implementation.

This is installed RyeOS runtime knowledge shipped by the standard bundle. It
describes the authority visible to operators and authored integrations; it is
not a repository implementation plan.

## Authority and ownership

The root thread, root event chain, and `AdmittedLaunchCapsule` remain the only
execution authority. The launch capsule binds pinned project authority and the
exact subordinate `AdmittedPersistentSessionCapsule`. The subordinate capsule
binds executable, lifecycle, wire protocol, complete canonical
structured-session contract, local schema hashes, source closure, route/effect
ceilings, workspace authority, and resource limits.

Worker, session, command, approval, and observation rows are operational
projections. They cannot mint execution, project, credential, or publication
authority. Lillux's attached process identity is process-control authority.
Recovery reconciles it with both admitted capsules; it never reconstructs
authority from a mutable worker row or registry resolution.

Worker process history is append-only across boots: `(session_id, boot_epoch)`
is unique and a replacement receives `MAX(boot_epoch)+1`. Dead/reaped or
dead/unproved rows remain as exact cleanup evidence. The dedicated-session,
credential-lock, and workspace ownership compare-and-swap transaction admits
at most one current worker; recovery cannot erase or reuse a prior epoch.

Every profile and worker-execution entry point admits only the exact configured
operator fingerprint. Authenticated requests may use local or remote transport;
local transport is not the predicate. Owner rows are defense in depth and do
not claim hostile multi-principal isolation.

Normal remote orchestration is node-to-node and therefore changes the acting
principal to the source node. An operator-owned durable workflow cannot use
that identity for its principal-scoped HEAD and later control. The generic
remote push/run seam has an explicit configured-operator mode: the incoming
request must already authenticate as the exact configured operator, the daemon
then signs the outbound request with that same configured operator key, and
the destination must authorize it with a node-signed, exact-scope
`remote_operator` grant bound to the source site's canonical ID. The source
node also co-signs the exact request, and the destination accepts that proof
only from a separately admitted `remote_node` grant carrying
`ryeos.attest.request.forwarded-operator`. The principal remains the operator
while authenticated transport origin remains a separate remote fact, so
local-only APIs still reject it. A grant is keyed by
fingerprint; on the hosted target this classifies every request signed by that
key as remote and rejects it without the source-node proof. This is not
caller-principal impersonation or a provider exception; delegated callers
cannot select it.

## Closed structured-session protocol

`ryeos.structured-session` is a fixed Rust protocol family, not a plugin or
general transformation language. Signed worker data can only select and narrow
its admitted vocabulary. Admission parses the full profile, rejects unknown
fields, compiles every local JSON schema, rejects remote references, validates
bounded routes/templates/predicates/observations/server requests, and embeds
the complete canonical contract plus schema hashes in the subordinate capsule.
The bridge executes only the exact captured profile digest.

Kinds own persistent-session resource defaults and ceilings. A worker may
author the closed `session_resources` mapping only when its kind declares the
override path; admission rejects unknown fields or values above the kind cap
and freezes the effective limit into the subordinate capsule. On Unix,
`real_uid_process_limit` becomes `RLIMIT_NPROC`, which is shared across the
daemon's real UID and is not a per-worker descendant quota. Session-group and
node-wide worker counts remain separate RyeOS controls.

Public commands contain an admitted route ID and schema-validated payload.
Direction, audience, effect class, fixed/workspace parameters, forbidden
authority fields, response predicates, retention, ceremony effects, and remote
session binding are capsule-bound. Clients never submit upstream methods or
RyeOS control frames. Runtime resume/read routes are not public. Unsupported
semantics require a reviewed Rust capability; authored data cannot become code.

Live command responses and durable execution results are separate contracts.
A kind may declare one optional composed result-policy field with the exact
generic vocabulary `full` or `digest_only` only when its terminator advertises
the closed `durable_result_projection` capability. Other terminators are
rejected until their terminal executor implements and declares that mechanic.
Absence means `full`, and only an effectively trusted signed item may select
`digest_only`. Root admission freezes the resolved policy and its
item/kind-schema identity into the sealed request. The current in-process
service terminator implements the capability: it still returns the full live
response or error to its attached caller, but its durable terminal stores only
the canonical response/error digest and frozen policy identity. The core
worker-command and remote-run services use this generic contract so a
confidential structured-session response is not copied into either the target
command thread or the source remote-execution thread.

The inherited target socket is full duplex. A persistent reader demultiplexes
responses and pushed observation batches while independent bounded queues keep
control moving. Batches bind session, worker, boot epoch, sequence, and digest.
RyeOS appends canonical facts to the root chain before acknowledgement. Stale,
duplicate, uncorrelated, unknown, or over-budget output cannot advance authority.
Pushed observation cardinality is bounded per worker event and by an exact
serialized-byte ceiling; ordinary commands and the two-route recovery control
have separate admitted aggregate ceilings. A hard cumulative event ceiling
applies across every worker epoch of one hosted session. SQLite retains one
cumulative settled predecessor frontier plus any exact ambiguous outbox body;
complete batch testimony remains solely on the root chain.
Root-fact idempotence is accelerated only by a bounded process-local index
derived from one complete authoritative replay plus every subsequent replayed
tail. Its Bloom filter proves absence only; an evicted or possible hit falls
back to complete root replay. It never consults a mutable projection as
testimony, and restart merely pays the one-time replay cost again.

## Lifecycle and evidence

Process lifecycle is separate from session projection, command contact, the
orthogonal approval set, and workspace disposition. A completed project session
closes and freezes its CoW candidate, then waits for explicit validation and
publish or discard while the root stays running. A projectless enrollment
session has no candidate and becomes terminal directly.

For pinned-CoW execution, the managed root process initially owns the active
workspace. Dedicated attachment atomically hands that exact process identity to
the held worker only when it still equals the root runtime identity; ready
workspaces retain the ordinary direct attachment path. Stale or unrelated
active identities fail closed. The controller waits on a dedicated bounded UDS
long-poll, so pushed projection changes neither poll SQLite nor monopolize the
shared callback connection.

Command contact is root-testified-before-write. The root receives the exact
`daemon_reserved_io` possible-contact fact before SQLite advances to dispatched
and before the socket write. A committed command in a dead epoch
without contact becomes a stable retryable-uncontacted failure; contacted or
ambiguous work is never replayed. Root facts retain canonical command,
idempotency identity, route/request digest, boot epoch, subordinate capsule,
profile, and schema identities. A successful response first appends one
canonical redacted command-observation batch to the root chain; only then are
events, approvals, session observations, and the result projection advanced.
Restart can therefore rebuild a dispatched command from that batch instead of
incorrectly downgrading authoritative success to outcome-unknown. A terminal
root is classified from its closed chain without attempting an impossible
append: no contact fact is uncontacted, contact without a response batch is
unknown, and a response batch proves completion.

Approval consent covers one exact action inside the admitted ceiling. It never
expands authority. The outbox reserves the decision, writes its root
possible-delivery fact before advancing the SQLite contacting projection and
before socket write, and distinguishes settled from delivery-unknown. Startup
idempotently completes missing decision/contact/unknown root facts without
refiring possible contact. Listing approvals is read-only. A signed workload
profile must mark an approval class deny-only unless its accepted upstream
effect is proven to remain inside the identical frozen permission profile;
displayable fields alone are not proof that consent preserves the ceiling.

Worker facts are `worker_asserted` or `upstream_reported`, not proof of success.
Reserved I/O boundaries are `daemon_reserved_io`; observed responses are
`daemon_observed_io`; candidate checks are
`filesystem_verified`; publication is `owner_authorized`. Upstream account-plan
metadata is testimony, not entitlement proof.

## Credentials and recovery

RyeOS owns an opaque mode-0700 profile home and generation/operation lock.
Before a worker is attached, the daemon validates the stopped home as one
bounded tree through pinned descriptors: links are counted but never followed,
special entries, multiply-linked regular files, mount crossings, and resource
limit violations are rejected, and every opened regular file and directory is
owner-private. A live workload may legitimately mutate its state tree while an
IPC request is being handled, so the bridge does not claim that concurrent tree
is a stable snapshot. Instead it reasserts owner-only access on the exact pinned
home root at initialization and every IPC boundary; the child also inherits an
owner-only creation mask. RyeOS-owned paths within the home require their
declared exact type and never accept links. There are no provider-specific
filename exceptions.
For immutable-argv profiles, the declared compatibility file is atomically
reset before each process generation and is never treated as policy. An
enforced isolation backend may additionally overlay it read-only.
The pinned workload owns its supported file credential format and refresh;
RyeOS never parses or journals opaque provider secrets. One active session per
profile serializes login, refresh, logout, revoke, and restart. Credentials are
plaintext node-private state visible to the configured operator.

Login is projectless and generation-bound. Device material uses only the
confidential live response lane; the surrounding recorded command and remote
execution services retain only a digest under their signed result policy.
Owner confirmation of sanitized account identity precedes project use.
Cancellation, expiry, disconnect, and restart invalidate the ceremony and
allow a fresh login epoch.

Revocation enters durable `revoking` before reaping workers or removing the
home. Admission, attachment, readiness, command, approval, and recovery recheck
the exact generation and lock. Unproved death retains worker identity and the
credential fence; the home is removed only after every worker proves cleanup.
Cleanup and lock release are transactional or resumable at durable boundaries.
One generic per-profile operation coordinator covers start through readiness,
every worker contact, termination, confirmation, revocation, and deletion.
Root ownership is always acquired before profile ownership.

In-memory retirement returns distinct `reaped`, `unproved`, `reserved`, and
`absent` evidence. Reservation or registry absence is never process-death
proof. If attachment fails after spawn, the exact Lillux process identity is
persisted as unproved before control returns; failure to persist that evidence
keeps the credential lock fenced.

## Workspace and publication

Project sessions require root-capsule `PinnedGeneration` plus `Cow` authority.
An execution that will explicitly publish a retained result starts from the
owner's existing principal-scoped project `HEAD`; this preserves the exact CAS
boundary later consumed by publication. Capture-live pinning remains a valid
private execution source, but its newly captured parentless snapshot is not an
existing `HEAD` and therefore is not the publication source for this workflow.
Workspace IDs and candidate rows are projections. Completion never publishes.
After the worker and managed controller have stopped, RyeOS freezes the exact
workspace generation, closes the private workspace, appends
`hosted_candidate.captured` to the still-live root, and only then exposes the
candidate projection. One root-operation lease covers close, fact, and bind.
That fact binds the candidate, admitted base/capsule, workspace, and credential
generation. The already-closed snapshot remains available in CAS for
validation and publication; validation, publish/discard, and only then root
terminalization follow on the same chain.
`validate-candidate-closure-and-base` proves canonical closure and admitted-base
ancestry only; project tests remain ordinary executions.

Publication additionally requires `ryeos.write.project.live`, the exact
principal key/project hash and expected base retained in root authority at
admission, owner authorization, and HEAD CAS. An owner-authorized root
reservation precedes HEAD contact; startup recovery requires that reservation
and appends a separately linked filesystem-verified result. After possible
contact, `HEAD == base` proves no publication and is the only retryable state;
`HEAD == candidate` proves success. A missing or different HEAD is
irreducibly ambiguous, receives authoritative `publication_unknown` testimony,
and terminalizes without retry. Root
terminalization waits while
publication may have contacted HEAD. A process-local root-operation lease
fences every hosted root-chain mutation; terminalization closes admission and
waits on its condition variable rather than polling SQLite. Pinned CoW worker
executions admit a retained-result authority (including the exact explicit
current-HEAD destination where applicable); projectless executions admit
exactly `any`. Discard/automatic-advance launch authority is not accepted by
this release.

For a runtime that declares native resume, a proved-dead launch owner does not
discard an unpublished CoW workspace. Startup retains the exact workspace
journal, verifies its backend, mount, and pinned root identities, and transfers
it only to the same thread's new launch claim. Immutable item/config resolution
is rebuilt from the admitted base snapshot in CAS; mutable workspace bytes are
not re-admitted as engine configuration. A crash during transfer is retryable
because owner replacement and stale process-attachment removal are one
transaction.

If restart occurs after candidate capture, startup first closes any interrupted
freezing workspace, repairs the missing root-fact-before-projection boundary,
and runs only the generic in-process disposition controller. It does not
restart or reattach the external worker to already-frozen mutable bytes. The
controller waits on pushed projection changes, reconstructs the canonical
generic session result after owner disposition, and commits the terminal root
event; candidate exposure is permitted only after workspace closure.

## Explicit non-claims

This substrate release does not provide hostile multi-principal containment,
provider-only egress, per-worker descendant quotas, worker pooling, invocation
leasing, cross-session reset, federation, or RyeOS local inference.

See also `knowledge:ryeos/core/kinds/worker` for the generic authored worker
kind.
