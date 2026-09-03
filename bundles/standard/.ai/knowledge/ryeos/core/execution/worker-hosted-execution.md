<!-- ryeos:signed:2026-09-03T13:38:40Z:144f1497b41169e0390ca4286b9f3780077c3e512a59e837b5a16f8938c0bc43:6PdPIo7gtO4tQgdHOXVn3pZTNxNPlgJWFXOPXs+Xun8AMAk+nYWVI+/uG4+ceLjT7aZQLe5Nynf++fLfqnneAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/core/execution"
name: "worker-hosted-execution"
title: "Worker-Hosted Execution"
description: "Implemented authority, protocol, lifecycle, recovery, and publication contracts for session-bound hosted workers"
entry_type: reference
version: "1.5.0"
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

The stable public identity is `chain_root_id`; each current or historical
placement is identified by `placement_thread_id`. Worker process history is
append-only across boots: `(placement_thread_id, boot_epoch)` is unique and a
replacement receives `MAX(boot_epoch)+1`. Dead/reaped or
dead/unproved rows remain as exact cleanup evidence. The dedicated-session,
credential-lock, and workspace ownership compare-and-swap transaction admits
at most one current worker; recovery cannot erase or reuse a prior epoch.

Every profile and worker-execution entry point admits only an exact
node-admitted operator principal: either the node's configured local operator
or a node-signed, origin-bound `remote_operator` grant with verified
source-node forwarding proof. Authenticated requests may use local or remote
transport; local transport is not the predicate. Owner rows are defense in
depth and do not claim hostile multi-principal isolation.

Normal remote orchestration is node-to-node and therefore changes the acting
principal to the source node. An operator-owned durable workflow cannot use
that identity for its principal-scoped HEAD and later control. The generic
remote push/run seam has an explicit configured-operator mode: the incoming
request must already authenticate locally as the source node's exact
configured operator, the source daemon then signs the outbound request with
that same local key, and the destination retains only its public key in a
node-signed, exact-scope
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

Portable placement is deliberately different from public operator forwarding.
An owner-authorized source operation creates the immutable handoff job, while
preflight, prepare, adopt, abort, bounded closure reads, and remote follow
delivery authenticate the exact configured peer as `remote_node` under narrow
service scopes. The target derives ownership only from the source-node-signed
chain/head and launch ledger, then binds later calls to its own signed preflight
and durable job. The transport node never becomes `requested_by`. Keeping this
internal path node-authenticated lets a normal local operator endpoint receive
a return handoff and lets crash recovery proceed without replaying a user
request or forcing one operator-key grant into incompatible local and remote
semantic classes.

For a bidirectional placement peer, the current exact node-grant ceiling is:

```text
ryeos.execute.service.objects/get
ryeos.execute.service.objects/closure/get
ryeos.execute.service.worker-placements/preflight
ryeos.execute.service.worker-placements/prepare
ryeos.execute.service.worker-placements/adopt
ryeos.execute.service.worker-placements/abort
ryeos.execute.service.federation/follow-terminal-deliver
```

Deployment may narrow a one-way peer to the subset it receives. These scopes
belong to the peer node key, not the configured-operator grant.

## Managed external-content activation

Large or third-party workload bytes remain outside signed bundles. A trusted
installed-bundle `config` may carry the closed
`ryeos.external_content_activation.v3` acquisition recipe: exact HTTPS archive
URLs and digests, signed archive entry/byte bounds, a consumer ref, closed
component shapes, and storage tiers. The `mapped` shape supplies exact selected
regular members: a file consumer requires one untargeted member, while a tree
consumer requires canonical targets and contains only those files plus their
required parents. The `whole_archive_tree` shape strips one canonical prefix
from an already-final publisher archive and admits only bounded directories,
regular files, and internal relative symlinks. Hardlinks, sparse files, special
entries, collisions, escaping paths, and undeclared transforms are refused.
The resulting existing manifest must still match the consumer's signed pin.
The recipe does not carry commands, scripts, host paths, policy, credentials,
component kind, manifest schema/hash, or mount authority.

`ryeos external-content activate <config-ref> <online|offline>
[offline-archive-root]` resolves and retains that signed recipe and its trusted
consumer. The consumer's existing external-content declarations remain
authoritative for component IDs, kinds,
pinned manifest hashes, and mounts. Node policy independently owns whether
online acquisition is enabled, the exact HTTPS host allowlist, redirect,
byte/archive-entry/concurrency ceilings, cache/store budgets, free-space floor,
and retries. Redirects are disabled at a zero ceiling or followed only through
canonical HTTPS destinations whose hosts are separately admitted by that same
node policy; every final byte remains bound to the source's signed digest.
Every resulting component capture must also fit the node's ordinary import
depth, entry, per-file, aggregate-byte, store-budget, and free-space ceilings;
the durable operation retains both policy tranches as one exact digest.
Acquisition stages mapped members or a closed archive subtree through Lillux's
descriptor-relative filesystem boundary, then feeds the result through the
existing content capture. Existing manifests and consumer binding heads remain
the sole launch authority.

Offline activation is explicit and never falls back online. Without a root it
uses only exact digest-keyed archives already in the node-private managed
cache. With a root it resolves one named root from node policy, binds that
root's policy authority digest into the durable operation, verifies the exact
path/device/inode through Lillux, and opens only each signed URL basename as a
no-follow regular file. The signed byte bound and digest are verified before an
atomic digest-keyed cache publication. It does not scan, transform, select an
alternative, or copy bytes directly into internal state.

The existing durable sync-job/attempt machinery owns retry and restart
recovery. The durable operation freezes the exact node-signed configured-
operator grant digest; revocation, scope replacement, or local/remote class
conversion terminalizes recovery instead of silently preserving authority.
Submission durably creates the job and its exclusive running attempt before it
returns. The command therefore returns a prompt `job_id`/`running` coordinate;
it does not hold an ordinary service execution open while network, archive, or
import work proceeds. `service:sync/jobs/inspect` observes that coordinate.
After terminal completion, repeating the same activation returns the verified
`completed` result and receipt idempotently. A daemon restart settles an
interrupted running attempt, then the existing recovery owner claims the same
canonical operation within its admitted attempt ceiling.
Completion publishes one compact node-signed receipt containing the activation/
program/policy/node/operator identities and sorted component-to-binding hashes.
That receipt is audit/recovery testimony; manifests and binding heads remain
launch authority. No public assembly directory, workload-named app root,
lasting named-root placeholder, shell extractor, or second realization format
is introduced. The exact configured operator may invoke activation locally or
through the origin-constrained configured-operator forwarding path; arbitrary
named-root import remains a separate local maintenance surface. The optional
offline archive root is only acquisition authority for the same signed recipe,
not consumer binding or workload filesystem authority.

## Portable environment selection

A project worker execution selects its signed portable environment through the
runtime-declared `environment` ref binding. The selector is not an ordinary
parameter and cannot be smuggled through the worker input envelope. The generic
launch preparer accepts only a trusted bundle/project `config` with the closed
`ryeos.worker_environment.v2` schema, derives the exact worker dependency from
it, and retains the engine-resolved path-free binding record in the outer
admitted program. The environment may additionally declare locator-free pinned
external content and an ordered executable-search list over those exact tree
realizations. RyeOS admits and materializes those declarations through the
existing external-content authority; the bridge receives only descriptor-
rooted search directories and never inherits an ambient host `PATH`. Changing
the config bytes at the same canonical ref therefore changes
`exact_program_hash`.

The outer program projection classifies every sealed invocation field. It
retains executable semantics, trust, exact source content, composed resolution,
execution hints, raw ref names, and resolved ref-binding identities. It excludes
local winning paths, resolver diagnostics, project materialization paths,
principal/site placement, launch mode, request parameters, validation mode, and
chain-retention policy. A new sealed field is refused until explicitly assigned
to the program or invocation side. Managed launch validation also proves that
the binding records in the retained execution closure equal those in the exact
program; direct execution cannot carry them.

The environment config is authored selection, not a third admitted capsule or
a credential container. The complete portable program is the existing outer
launch program plus each named persistent-session dependency program and their
typed closures. Node-local credentials, profile generation, process capsule,
execution realization, workspace path, and callback authority remain placement
state.

## Portable checkpoint and placement

A portable checkpoint is admitted only after the exact placement is frozen,
its last worker process is proved reaped, every command and approval contact is
settled, the credential profile is active and unlocked at the exact generation,
and no provider attempt or unpublished accounting testimony remains. The
checkpoint is an ordinary `StateManifest` whose typed restore document binds
the stable `chain_root_id`, source placement and event, outer exact program,
named persistent dependencies, project candidate authority, settlement digest,
credential-subject projection, and source site.

The public lifecycle makes that boundary explicit. `terminate` accepts only
`completed` or `cancelled`: completion freezes a project placement and permits
candidate disposition and checkpoint capture, while cancellation terminalizes
without a resumable checkpoint. Same-node `resume` conditionally installs the
exact manifest into a fresh placement under the stable chain root; it never
releases a successor worker against mutable or unclassified predecessor state.
The successor launch metadata durably records that this is externally restored
state. The outer managed runtime therefore cold-starts without copying a native
predecessor checkpoint or receiving `RYEOS_RESUME=1`; restart recovery consumes
the same recorded bootstrap mode. Ordinary machine continuations that delegate
state recovery to their managed runtime instead record predecessor-native
checkpoint bootstrap. This is a generic execution distinction, not a worker- or
provider-kind branch.

Workload-owned portable state is selected by the closed contract frozen in the
persistent-session capsule. RyeOS captures only matching files into a canonical
portable-state tree. Credential files and values, unrelated workload sessions,
and unselected profile-home bytes are excluded. Restore is conditional on the
exact predecessor manifest/tree and changes only admitted selector roots before
any successor process is released. A target selects its own owner-authorized
credential profile and exact generation; only the domain-tagged, signed
credential-subject digest crosses sites.

Cross-site handoff is a cold continuation, not filesystem or process migration.
The source first completes and freezes the placement, proves its exact worker
reaped, and publishes the authoritative portable checkpoint. It then resolves
one directional configured full-project route and obtains a target-signed
preflight bound to that frozen source head and exact proposed successor. A
preflight issued before checkpoint publication cannot authorize handoff because
checkpoint publication advances the source chain head. Typed sync jobs retain
staged closures and every recovery coordinate. The target verifies the complete outer/dependency programs,
checkpoint, project base/HEAD, local credential subject/generation, accounting
ceiling, and node policy, then signs the final placement admission without
releasing a process. Target accounting accounts remain non-spendable while
prepared. After target admission, the source rechecks the settled ledger and
commits an externally anchored debit for the exact target caps. Its immutable
allowance-transfer receipt is rooted by the source-signed writer grant and
continuation. The source then atomically terminalizes its placement and creates
one remote continuation under the same `chain_root_id`; only then may the
target adopt that chain head, activate the exact prepared allowance inside the
adoption/runtime-install critical section, conditionally install state, attach
its held process identities, and release the new placement.

The target also resolves an exact `AdmittedOperatorAuthority` from its current
node-signed `remote_operator` grant for the owner and immutable origin site.
That authority binds the principal class, owner principal, configured origin,
grant digest, and sorted canonical scopes. It must cover both the target
capsule's retained effective capabilities and its required-nullable parent
delegation ceiling. The target seals it into the target launch capsule and
placement evidence, then revalidates the identical current grant during
preflight replay, preparation, immediately before placement publication, and
again before private-state installation or runnable recovery. Revocation or
any changed grant bytes therefore fences every path that can launch a worker
or open credential-private state; equivalent re-authoring is not a replay of
the sealed grant. Once a target-signed terminal receipt exists, replay of that
exact historical settlement instead uses immutable placement, request,
accounting, and receipt testimony. It still authenticates the source node, but
cannot launch a worker or access credential-private state and therefore does
not depend on a later mutable operator grant or placement lease. This owner
authority is distinct from the peer `remote_node` grant used for closure
transfer and autonomous placement calls.

The source allowance export is the distributed financial commit point. Before
it, an aborted handoff closes unused target preparations and leaves source
allowance intact. After it, recovery must complete the exact writer cut and may
never refund, abort, or reactivate exported source allowance. Graph-followed
placements move only their finite directive slice and leave the source
execution account active with a durable transfer debit. A directive-free
execution root may move its whole remainder only with no other open launch
gate, and the source account then closes. Unbounded-to-unbounded transfer is
refused. Both reservations and exports subtract prior transfer debits, which
prevents source/target double spending under concurrent admission.

The historical source `AdmittedLaunchCapsule` remains an immutable object in
the transferred chain closure; it is not erased or rewritten when current
placement ownership moves. Its complete sealed invocation is the sole source
launch input at the target. The target decodes that typed capsule, preserves
its exact program, lifecycle, effective capabilities, and required-nullable
parent-delegation ceiling, and applies only the attested project, site, and
credential-profile rebind before minting a new target capsule. Source
`RuntimeLaunchMetadata`, source checkpoint directories, source isolation
attempts, handler authentication, cancellation policy, and other node-local
runtime fields never cross the site boundary. Handoff v1 refuses a source with
a non-null cancellation policy because no portable contract roots it.

Every closure fetch and staged handoff payload is bounded by the consuming
node's mandatory `object_closure` policy. The serving node independently
enforces its own policy. Handoff code carries no Codex-specific or fixed
transfer allowance, and a caller-supplied limit may only narrow node policy.

Every possible target must independently activate the exact non-secret worker
realization before preflight; equal program identity does not make source-local
realization paths portable. The owner principal's target project HEAD must
already equal the source placement's immutable base generation. Ordinarily the
origin retains that base. When two configured-operator HEADs are valid but
divergent, `remote reconcile-project-head` requires both exact observed hashes
and an explicit content winner, then publishes one two-parent generation to
both nodes through a durable remote-first job. Launch the new placement from
that shared generation before attempting handoff. Placement preflight never
overwrites or silently rebases a divergent target HEAD. Each target also
selects an independently authenticated node-local credential profile. Only the
signed subject digest crosses sites.

Target preparation atomically couples its credential-generation reservation
to a durable fence over the exact owner-principal/project/target-HEAD tuple.
Every online HEAD writer, including snapshot creation, push/reconciliation,
managed fold-back, and compact GC, serializes with that reservation. A changed
HEAD is refused before placement publication; no other writer can change it
between that recheck and the source's irreversible writer cut. Pre-cut abort
releases the credential reservation and fence together. Successful or
recovered target adoption releases the project fence only after the
authoritative target branch is published; a crash before that point leaves it
active.

The continuation event binds `origin_site_id`, source and target sites, source
and successor placement threads, both signed chain heads, checkpoint and
placement attestations, project rebind, the exact rooted accounting transfer,
and any retained
follow-delivery reservation. Source and target durable jobs recover their own
side of every crash gap. A failed pre-commit transfer leaves the source current;
after the continuation commit the source cannot reactivate and target recovery
owns completion. Routing follows the signed current chain head, so a stale
placement thread or boot epoch cannot accept commands.

Target adoption has four mutually exclusive, node-signed terminal branches
under one permanent operation head: attached successor, source-authorized
abort, proved completion before attachment projection, or proved terminal
failure before attachment. Completion never fabricates a `ProcessAttached`
event; the target instead proves the exact completed terminal chain, dead and
reaped process identity, and released historical credential lease. Source
recovery imports that complete signed closure and advances the same chain. A
later local credential generation or owner may coexist with this historical
proof, but the old worker may not still own the current lease.

Handoff contact retries are logically unbounded because an offline peer must
not permanently strand already-transferred authority. While the operational
job is retained, its cumulative attempt count never decreases; SQLite compacts
only the newest bounded suffix of terminal attempt diagnostics plus at most one
running reservation. Ordinary terminal-job GC may later remove that operational
row and its attempt diagnostics. Permanent target branch heads and signed
terminal testimony—not the sync-job row—preserve settled handoff authority.
Every error after reservation—including response decoding, closure fetch, and
signature validation—settles that reservation before another exact redrive may
begin. Each authenticated peer interaction has a finite total wall-clock bound;
expiry settles the current reservation and leaves the same exact logical
redrive eligible. Sync-job inspection reports the cumulative count, retained row count,
retention mode, and terminal-row ceiling explicitly; consumers must verify the
exact newest consecutive suffix rather than mistake diagnostic pruning for
lost logical attempts.

This federation contract needs no global session registry, shared filesystem,
identical app-root paths, host-local node-instance ID, scheduler, or transparent
migration layer. Each app root remains one complete node; authenticated RyeOS
site/node identities and signed chain placement are the cross-node boundary.

## Generic session client

`ryeos worker session status|command|command-observation|approvals|approval|terminate|checkpoint|resume|handoff-preflight|handoff|validate-candidate|publish|discard`
are signed Core command descriptors over the existing generic worker-execution
services. Every operation begins with `chain_root_id`, resolves the authoritative
current placement, and then fences placement thread and boot epoch internally.
The historical command-observation read additionally requires the exact
`placement_thread_id` because command sequence is placement-local and may recur
after handoff. It verifies that placement's retained command and turn facts
without redirecting the query to the current chain head.
Historical catch-up uses chain replay and live attachment uses the existing
cursor-based chain event stream. Attach and detach are client behavior: opening
or closing that stream creates no session row and mutates no worker authority.

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
node-wide worker counts remain separate RyeOS controls. Linux does not enforce
`RLIMIT_NPROC` for real UID 0, so a root-run node records and applies the
configured value but cannot claim it as an effective fork ceiling. Deployments
that require mechanical process containment must run the daemon under a
non-root service UID or use a separately proven cgroup/process-isolation
boundary; disabled isolation with a root daemon remains trusted signed
execution, not hostile-worker containment.

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

Operational state locks are exact parent-process authorities. Lillux registers
each held lock before opening it and closes that descriptor in a forked
attachment child before the pre-exec hold, so a worker cannot retain the
daemon's lock across a crash. Normal daemon startup may wait for at most five
seconds for kernel teardown of the predecessor generation; it never steals or
replaces a live lock. Offline and standalone access retains immediate
fail-closed acquisition.

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
unknown, and a response batch proves command completion, not completion of an
asynchronous turn it may have started. An exact retry of an already
settled command is a read of that retained authority only after RyeOS verifies
the projection against the exact committed-command testimony and its settlement,
verified-uncontacted failure, or admitted response-batch fact. RyeOS resolves
that proved replay before root appendability, credential, and worker-contact
admission, so the same retained result remains available after terminalization without
reopening history or contacting the retired worker. A new or unsettled command
still follows the ordinary appendability gate and fails closed on a terminal
root. Reuse of the idempotency key for different authority is rejected before
that gate.

A successful command response and a later asynchronous turn terminal are two
different settlements. Every admitted `idle -> turn_running` and
`turn_running -> idle` observation therefore emits a deterministic
`hosted_session.turn_started` or `hosted_session.turn_completed` fact in the
same authoritative append as its command or pushed-observation batch. Start
testimony binds the exact placement-local command sequence and request digest.
Completion testimony is independently keyed by placement, worker epoch, and
turn. The projection joins the two exact unique facts and rejects duplicate or
reused turn identities rather than relying on mutable status. The owner-authorized
`command-observation` service joins those immutable facts by exact chain,
placement, and command sequence after live status has returned to idle, after
restart, and after command replay. Mutable session status never retains a
`last_completed_turn_id` and cannot grant historical completion authority.
Completed termination may carry the returned completion fence. Under the
existing root-operation lock RyeOS revalidates the exact capsule, command,
request, turn, completion fact, and that turn's originating worker epoch. A
recovered worker may have a new boot epoch, but termination is refused when the
placement's owner-route command frontier has advanced.

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

For a followed worker moved away from its graph parent, the parent site signs a
reservation before the first transfer. It binds the exact parent chain/head,
follow waiter and successor, child item/specification, initial child thread,
stable child chain root, owner, and parent node/site. Every later placement
retains that same attestation; an intermediate source cannot replace it.

When the followed child terminalizes on another site, that site signs the exact
terminal chain head, event, status, complete managed terminal envelope, and
reservation hash. A target-owned retryable sync job delivers the attestation
through exact-scope authenticated node transport to the original parent site.
The parent fetches and stages the complete target closure, verifies it is an extension of
its retained pre-handoff child head, rechecks the signed reservation against the
live waiter and parent chain ancestry, appends one idempotent delivery fact to
the dormant successor, and then uses the existing follow-resume path. Startup
reconstructs a missing target delivery job from the authoritative terminal
chain, and both parent and target jobs make fact-before-projection and
projection-before-kick crash gaps retryable. A handoff back to the parent site
uses ordinary local follow settlement instead of a remote delivery.

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
When a project-backed parent will spawn independently dispositioned child roots,
its signed execution policy may select `cow_retain_result` for child
realization (`ryeos execute --retain-child-results`). Each child then receives
its own private pinned-CoW result with no inherited project-HEAD destination;
validation and publication/discard still require an explicit owner operation.
The separate generic `--no-operator-vault` control narrows a project-overlay
environment by removing its node-private operator-vault leg. Neither control
names or detects a worker kind, provider, graph, or workload profile.

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
leasing, cross-session reset, a scheduler, live migration, simultaneous active
placements, a workload-native remote-client gateway, or RyeOS local inference.

See also `knowledge:ryeos/core/kinds/worker` for the generic authored worker
kind.
