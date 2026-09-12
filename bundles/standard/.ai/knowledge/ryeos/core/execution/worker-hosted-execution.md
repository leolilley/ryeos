<!-- ryeos:signed:2026-09-12T07:36:30Z:1f888e8720da3d2b75804e2a1244ef80f14456b7011a027da01bb18efe76a2d1:8+WmjbfyIKXFw8i84HP8u3bf+ECBQrGDC9SfPzPjUinMp8Tu7gIg9o3AjpKnXq3xq6L0H99B2lyJVY/X9kKBCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/core/execution"
name: "worker-hosted-execution"
title: "Worker-Hosted Execution"
description: "Implemented authority, protocol, lifecycle, recovery, and publication contracts for session-bound hosted workers"
entry_type: reference
version: "1.8.0"
```

# Worker-Hosted Execution

Worker-environment v6 explicitly includes `external_product_slots`, empty for
literal-only environments. A pending product slot names a signed relationship
Config and realization shape, not a digest or executable grant. The pure launch
preparer checks combined literal/slot search and environment references; the
local operator's product selection must be verified and ordinarily bound before
the prepared Config dependency can be realized. Dynamic slots are project-only;
installed bundle environments continue to use literal pins.

The v6 contract combines product slots with explicitly selected CLI and/or
structured-session invocation bindings. The two earlier v5 branch shapes are
not accepted as this combined contract. Source integration and separate prior
acceptance runs do not qualify a newly built combined generation.

Worker-hosted execution runs one long-lived subordinate workload for one
ordinary RyeOS root execution. It is generic execution substrate, not an
engine kind, agent identity, or local-inference implementation.

A persistent execution dependency is not a separate thread root. Its direct
plan consumes the dependency's existing finalized effective program: the
complete composed subject supplies the registered kind's filesystem/network
projections, while retained root bytes supply parsing. It must not require or
fabricate a second root admission, borrow the enclosing runtime's composition,
or silently use node defaults because that dependency has no root thread.
Subject identity, enclosing restrictions and retained protocol narrowing remain
mandatory before the plan is sealed; recovery consumes that sealed plan.

Session admission reserves the exact pending worker ID and boot epoch before
process contact, including after recovery. Boot-local workload-client admission
must match that reserved tuple and credential generation; empty worker fields
are not an admissible pre-start state. This reservation fences cleanup but is
not liveness evidence: the separate held-process attachment still owns that
transition. Do not clear the pending identity to make client setup pass.

Held-launch failures carry cleanup authority separately from diagnostics.
Lillux retains the exact supervisor until its process group is quiescent and
the child is reaped; only an unconsumed attachment boundary can produce the
typed `AbortedProcess` testimony on a spawn failure. Engine dispatch preserves
that testimony, and the session owner consumes it through its existing
failed-start settlement. Refusal text, PID absence, and attempted kill/wait
are never cleanup proof. Missing proof keeps the credential/workspace fence.
This in-memory proof cannot retroactively settle a historical attempt which
lost its launcher identity; replay must retain that uncertainty.

Adopting a control channel at standard-input coordinate zero may leave that
coordinate vacant. The bridge's fresh workload pipes must still survive exec;
Lillux applies the same child-only stdio preservation used by its subprocess
runner. No ambient standard stream or dummy host file is substituted. Startup
diagnostics distinguish request/notification phases and may include the exact
child's OS exit status, but never upstream private stderr. Those diagnostics
remain separate from the daemon's cleanup testimony.

Bound source has separate daemon and workload coordinates. Daemon baseline
preparation reads the already-pinned, verified source generation under its
existing materialization lease. The execution entry path is only a target
namespace coordinate; its mount may not exist in the daemon namespace at all.
Baseline paths use the captured manifest root, matching profile compilation.
The enforced read-only overlay retains that exact file descriptor, not a
reopened workload path or a substituted live bundle file.

The daemon is the sole compatibility-seed preparation owner. The bridge
verifies the bounded, no-follow file against the admitted baseline before
starting the workload; it does not rewrite it after isolation has mounted it
read-only. Missing or divergent bytes refuse launch in every isolation mode.
The source mount's file mode is not a replacement for mount or immutable-argv
authority. Do not add a bridge-side seed repair or mount-error fallback.

Captured execution excludes ambient node-policy filesystem mounts, not the
session's explicit private-state grant. Both ordinary and held dispatch retain
that exact launch root. Isolation independently pins it as a strict child of
node state, rejects the node-state root itself and outside paths, and bounds
any read-only baseline overlay to that exact private root. No caller-selected
path, sibling profile, bundle root, trust directory, or daemon socket follows
from this grant. The same credential-generation and worker cleanup fences
continue to control which private home the daemon may supply.
Plan emission retains both admitted realization mounts and state-overlay
mounts with their ordered layers; validating an overlay without emitting it
does not enforce read-only protection. Verified code is emitted separately.

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

Public route commands select only public-audience routes in the root's frozen
route set. Unknown, unselected, or recovery-only routes are rejected before
command reservation and worker contact. Daemon-owned reattachment uses its
separate runtime surface. Exact settled command replay remains a read of its
original testimony, even after terminalization; it does not re-admit or resend
the request. Payload semantics remain governed by the admitted protocol.

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
`ryeos.worker_environment.v6` schema, derives the exact worker dependency from
it, and retains the engine-resolved path-free binding record in the outer
admitted program. The environment may additionally declare locator-free pinned
external content and an ordered executable-search list over those exact tree
realizations. RyeOS admits and materializes those declarations through the
existing external-content authority; the bridge receives only descriptor-
rooted search directories and never inherits an ambient host `PATH`. Changing
the config bytes at the same canonical ref therefore changes
`exact_program_hash`.

Portable content is not necessarily bundle-owned. A project environment's
consumer binding is keyed by the exact definition generation already admitted
by the outer launch, including a COW subject's current operational generation.
Static preview and launch use that same authority; neither a live path nor the
latest project HEAD can substitute for it. On another site, project content is
resolved under that admitted definition generation and compared with its
retained portable program before target-local binding admission. Installed
execution and content dependencies remain projectless: selecting them from a
project must not give their code a project overlay. Same-site recovery keeps
the captured realizations rather than re-resolving mutable names.

The v6 configuration may also declare `process_environment`. This is not an
extension of content authority and is not a project/vault environment overlay.
The kind-owned preparer emits a generic path-free environment contribution as
a sibling of execution and content dependencies. Every contribution names its
target execution dependency; a `realization_path` is compiled into a
`content_path` that additionally names the exact content-dependency key which
grants the realization. Literal and daemon-owned `runtime_view_directory`
values do
not require external content. The signed runtime descriptor independently caps
contribution, target, and variable counts. Generic admission rejects missing
targets, duplicate variables, absent content dependencies, target mismatches,
non-tree realizations, and paths whose retained manifest type differs from the
declared file/directory type.

The persistent-session capsule retains only the validated path-free process
environment, capped at 32 entries and 4096 serialized bytes. Placement prepares
a bounded delivery envelope on the existing protected environment channel;
the envelope is not authored configuration or additional content authority.
The session protocol must explicitly
allow the sealed `RYEOS_SESSION_PROCESS_ENVIRONMENT` relay through its existing
`runtime_env_allowlist`; otherwise admission fails. The receiving bridge clears
its inherited environment and deliberately installs only these resolved values
alongside its fixed minimal environment. Existing
`RuntimeEnvSource::EnginePlan`, subprocess composition, and node isolation
policy remain the final enforcement path. No host environment, absolute
authored path, credential home, project ignore entry, or kind-specific engine
branch becomes environment authority.

With enforced isolation, execution-runtime paths are usable by ordinary child
processes without preserving private descriptors. Lillux first proves the
exact pinned object and read-only namespace ancestors; the initial executable
still uses its held descriptor. The native backend seals only its synthetic
root, not separately admitted writable mounts. A readonly leaf below a writable
ancestor is insufficient and is refused.

For each `runtime_view_directory`, placement prepares the exact directory under
the borrowed workspace's `.ai/cache/ryeos-runtime` capture floor and mounts it
at `/ryeos/runtime-views/<ENV_NAME>`. The leaf's contents are writable; its name
cannot be replaced. The compiler refuses overlaps and the bridge proves that
the prepared source and target mount are the same directory. Missing or changed
sources refuse launch instead of triggering bridge-side repair. This supports
ordinary descendant cache use while keeping caches out of retained candidates.
Disabled-isolation delivery remains descriptor-bound; it does not claim the
immutable namespace guarantees of the mounted lane. Delivery mode is explicitly
prepared, never inferred from whether a mount happens to exist.

Persistent-session capsule v10 fences this prepared delivery contract. Earlier
retained bridges consume a different environment format and are refused rather
than translated during recovery.

The required-nullable v6 `workload_client` member is independent of process
environment and external-content authority. `null` disables it. A non-null
request selects explicit CLI, structured-session, or both ingress bindings plus
a finite, sorted set of child item refs, ref-binding values, call forms,
effect classes, signed workspace-access assertions, in-flight count,
invocation count and lifetime. The signed
worker-execution configuration separately supplies a delegation ceiling, and
the node execution policy may disable or further bound the feature. Admission
intersects the original verified caller scopes, root delegation ceiling,
project request, current node ceiling and each child kind's mechanically
derived exact execution capability. No item, provider, compiler, project or
workload name is engine vocabulary.

RyeOS does not expose an operator key, callback bearer, thread-auth bearer,
daemon address or ordinary CLI transport to the workload. The daemon retains
those existing authorities in memory and creates one boot-bound protected
target channel. Its secret-free boot frame contains the protocol, grant
digest, selected ingress, byte/concurrency/lifetime bounds and a finite public
operation presentation. The existing verified resolution and inventory owners
derive that presentation from the original admitted source, never candidate
edits. The placement's immutable `hosted_session.workload_client_admitted`
fact binds its profile, request and presentation digests before contact;
reattachment must reproduce the same recipe. It does not grant execution.

That public presentation uses the workload client's actual request grammar.
In particular, a default call is presented as a null/omitted `call`, and a
named call as `{method}` with its argument schema described separately. The
tagged enums retained in the private admission ceiling are authority
representation, not invocation examples, and must never be exposed as though
the workload could submit them. The daemon compares a deterministic public
authority projection with the retained ceiling before releasing the boot.

CLI binding requires an exact client member in a declared pinned tree.
Inside an enforced private-tmp and fresh
PID-namespace sandbox, the trusted bridge publishes one random owner-private
local endpoint. Lillux accepts only a non-init peer visible in that PID
namespace, while the client proves the connected server is namespace PID 1;
pathname replacement therefore cannot impersonate the retained broker. A
restricted client realization staged as `ryeos` supports only
`ryeos execute`; it has no app-root discovery, HTTP/daemon fallback, signing,
remote, lifecycle, installation or publication surface.

Structured-session binding instead uses the signed profile's closed
registration/request/result mapping over its existing application connection.
It requires no client executable, listener, endpoint environment or unused
realization. Selected interfaces share one grant, slot pool and daemon channel;
there is no automatic retry through another interface. A protocol callback
does not make an ordinary shell command a RyeOS execution.

The trusted bridge assigns ingress provenance. Protocol session/operation/call
identity is distinct from reply RPC identity and caller-controlled CLI IDs.
The existing `RuntimeActionIntent` retains that source and exactly one child;
a new call must match the current hosted turn. Same-call changed behavior is
refused. An old call under a new boot grant is fenced to the original action
and reports unknown rather than executing again. Child failure, unknown outcome,
and unavailable retained result remain distinct from task success. No ingress
may turn digest-only/unavailable results into permission to rerun.

Structured-session wire version 2 adds request-correlated progress
acknowledgements. A signed route can select one unconditional turn-start
notification from its existing profile. That notification's signed upstream
session pointer must select a required string in its schema and match the
already-bound session before any progress is admitted. The bridge sends it as
a Delta on the exact active command; it does not infer authority from a tool
call's turn ID. Under the original command lease, the daemon atomically records
`hosted_worker_command_progress` and `hosted_session.turn_started`, applies the
projection, then sends an ObservationAck with the exact request ID and canonical
progress digest. The bridge withholds both invocation dispatch and causally
later pushed batches until that acknowledgement; timeout or mismatch fails
closed. Ordinary uncorrelated observation acknowledgements retain their own
sequence/digest identity.

The final command batch must corroborate the same early boot, command sequence,
request digest and turn. It retains the start for historical lookup but does
not append or apply it twice. Recovery validates the original progress batch
and start source; an already completed turn is never resurrected by a delayed
final response. New child ingress is refused immediately once the existing
root gate is terminalizing, while earlier accepted work retains its causal
settlement lane. These are existing command/observation and runtime-action
owners, not a second invocation ledger.

The application event loop continues servicing controls and server messages
while a child is outstanding. Child settlement/thaw remains daemon-owned, so a
frozen bridge is not needed to release its own workspace borrower. Lillux bounds
channel I/O with absolute deadlines and supplies shutdown wakeup mechanics.

A nested workload permission profile may reopen only the fixed private broker
directory `/tmp/.ryeos-wc` as read-only beneath broader tmp-directory denies.
The random endpoint, bidirectional PID proof and daemon admission still bind
the usable surface to the exact outer worker boot. Whether the nested sandbox
can connect under that exact read-only rule is an installed acceptance gate;
failure must not be repaired by widening `/tmp` or moving the endpoint into a
project/runtime-view tree.

The endpoint locator is not a callback/thread-auth bearer or daemon address,
but possession lets a direct descendant inside the outer worker sandbox ask
to exercise the exact bounded boot grant. Every such request remains subject
to the daemon's complete live revalidation. RyeOS-dispatched child tools use
their own admitted clean environment and inherit neither that endpoint nor any
outer worker credential. This distinction must not be weakened into a claim
that arbitrary direct shell descendants cannot observe their parent's
environment.

The bridge only frames and multiplexes requests. The daemon validates the
exact live chain, placement, worker instance, boot epoch/identity, root and
session capsules, project authority, node-policy generation and retained grant
on every invocation, then enters the existing `runtime.dispatch_action`
handler. Existing callback storage, thread auth, `RuntimeActionIntent`, child
links, borrowed-child provenance, child launcher, effect handling and recovery
remain the only execution owners. Implementations must extend those owners;
they must not add a workload-client API, signing principal, token store,
operation ledger, child registry or launcher.

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
`ryeos remote worker pull-result [remote] <chain_root_id>` is the distinct
source-side command for returning a frozen retained project candidate; it
requires a configured full-project binding and never accepts caller-supplied
base or candidate hashes.
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

`limits.aggregate` is the shared execution-tree budget, not another worker
controller. Its current closed dimensions are one durable absolute duration
deadline, total logical worker executions, and bounded hosted-turn contacts.
They follow the daemon-minted accounting scope through descendants. Worker
recovery reuses its placement claim; an exact command replay reuses its command
claim; only root-proved uncontacted attempts release a contact. Possible or
unknown contact remains consumed. Finite hosted-contact limits are rejected
for interactive sessions because their open command stream has no equivalent
pre-contact meter.

This operational frontier is currently node-bound. A signed remote launch may
admit the whole beat directly on its target node, but cross-site migration of
an already-running worker is refused when its aggregate budget is finite or
consumed. The existing handoff transfer conserves financial allowance; it does
not yet partition distributed operational counters, and the target must never
restart them from its local account birth.

Other limits retain their narrower truthful authority. Token and USD limits
are enforceable only where a runtime/provider supplies those authoritative
observations; a subscription-backed hosted worker supplies neither and must not
claim them. Event, attachment, process, and per-command byte/cardinality caps
remain per admitted execution or worker. Combined with the aggregate logical
worker ceiling they provide a mechanically bounded whole-tree maximum, without
duplicating those ledgers as aggregate counters. Domain work units such as
simulator steps remain the responsibility of the signed workload contract.

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

Approval-required is equally fact-bound. Status and wait expose an exact
pending-approval fence only when the current placement, capsule, worker epoch,
turn, approval ID, request digest, unresolved approval row, and immutable
`hosted_session.approval_requested` root fact all agree. A bounded controller
may terminate as `approval_required` only with that fence. Terminal validation
rechecks the same fact both before worker shutdown and after cleanup has changed
the unresolved row to its stale-epoch state. A transient
`awaiting_approval` label, an expired request, or a request with a reserved or
possibly delivered decision cannot authorize that outcome.

A signed worker-execution profile may select `bounded_turn` instead of the
ordinary owner-directed `session` mode. That controller issues only its two
profile-fixed session-start and turn-start routes, observes the exact turn
command coordinate, and requests completed termination only with its durable
completion fence. Each step uses a typed, deterministic attempt key. Only an
exact daemon-verified `failed_uncontacted` settlement may advance to the next
bounded attempt; a contacted or outcome-unknown command is never redriven.
Fresh execution therefore normally uses command sequences one and two, while
restart recovery reports the actual sequences and boot epochs rather than
assuming that reattachment did not advance the ledger. Approval requests,
budget expiry, and ambiguous recovery fail closed.

These restrictions are daemon admission rules for both owner commands and
runtime callbacks, not only controller conventions. A new route must match the
sealed goal and signed attempt ceiling. Reservation and contact both require
an approval-free idle boundary with no unresolved observation projection;
observation ingestion serializes with the contact commit. Absolute worker and
aggregate deadlines also bound queued writes. Exact settled replay remains
readable after expiry without contacting the worker.

The bounded profile requires a pinned private CoW workspace, retain-result
capture, and `retained_for_review` candidate disposition. Its compact
terminal session projection is rebuilt from the command/root facts, workspace,
completion fence, and admitted launch capsule. It exposes every bounded attempt
coordinate, exact contact classification, base and candidate identities, and
the original completion boot epoch. Spend is reported as unavailable/null when
the launch has no authoritative financial ledger; subscription-backed or
external account metadata is never fabricated as RyeOS cost. Existing session
profiles remain owner-directed and retain their live-filesystem/projectless
operation where admitted.

## Candidate evaluation and integration

Canonical closure and admitted-base ancestry are necessary diagnostics, but
they never make a candidate publish-ready. Publication authority begins only
with an accepted result from an independent evaluator whose complete signed
definition closure was resolved from the exact immutable base generation. The
evaluator executes against the exact frozen candidate in a read-only or
CoW-discard view. Its separate root capsule seals both generations, the source
candidate/root/owner, parameters, external-content authority, and execution
limits; its terminal chain retains the exact result and contact evidence.

A bounded `retained_for_review` worker cannot publish its own candidate. After
the first accepted evaluation of candidate C, an owner may launch one separate
integration root. RyeOS resolves the signed authoring wrapper from base B and
runs it inside a private retained CoW view of C. `runtime.author_item` keeps the
signing key in the daemon and writes only that capsule-sealed workspace, under
the wrapper's signed item-authoring namespace. It does not write the live
project tree or advance HEAD. The completed integration root captures result
generation D and proves D descends C.

D must then pass a fresh independent evaluator run, again resolved from B.
Only the resulting C-evaluation, integration-root, and D-evaluation testimony
together authorize the serialized project transition. `publish` re-verifies
those immutable coordinates and advances the principal-scoped project HEAD by
one compare-and-swap from B to D. A stale HEAD fails closed. The terminal worker
root remains immutable evidence throughout; evaluator and integration work use
their own roots and ordinary restart-recoverable launch capsules.

This candidate lane is additive. Ordinary admitted `live_direct` execution and
daemon-mediated live-project item authoring remain available under their
existing explicit write authority; RyeOS never silently treats a live-tree
write as a CAS generation or HEAD publication.

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
Exact event attachments are available at their admitted `evidence/...` paths
relative to the worker workspace, including when node isolation is disabled.
These are execution inputs: native fold-back excludes their process-visible
bytes and preserves any original project files underneath the input paths.
They cannot silently become authored candidate content. An enforced isolation
backend supplies read-only mounts; disabled isolation supplies exact private
copies on the trusted node and does not claim kernel-enforced immutability.
After the worker and managed controller have stopped, RyeOS freezes the exact
workspace generation, closes the private workspace, appends
`hosted_candidate.captured` to the worker root, and only then exposes the
candidate projection. One root-operation lease covers close, fact, and bind.
That fact binds the candidate, admitted base/capsule, workspace, and credential
generation. The bounded worker root can then terminalize as
`retained_for_review`; it remains immutable evidence. Owner-authorized
validation, evaluation, qualification, integration, publish, and discard use
separate recorded service or execution roots that name the source root exactly.
`validate-candidate-closure-and-base` proves canonical closure and admitted-base
ancestry only; project tests remain ordinary evaluator executions and only
accepted immutable evaluator testimony can make a candidate publish-ready.

Publication additionally requires `ryeos.write.project.live`, the exact
principal key/project hash and expected base retained in root authority at
admission, owner authorization, and HEAD CAS. An owner-authorized publication
root appends the exact reservation before HEAD contact. Startup recovery
re-verifies that reservation. An exact rooted terminal result preserves the
classification made at contact even after later HEAD movement. If recovery
finds only the reservation, it keeps that same publication root appendable,
classifies the signed authoritative HEAD and history, and appends the recovered
terminal result before settling the controller projection: `HEAD == base`
proves no publication and is the only retryable state; `HEAD == candidate`
proves success. A missing or different HEAD is
irreducibly ambiguous, receives authoritative `publication_unknown` testimony,
and terminalizes without retry. Root
terminalization waits while
publication may have contacted HEAD. A process-local root-operation lease
fences every hosted root-chain mutation; terminalization closes admission and
waits on its condition variable rather than polling SQLite. Pinned CoW worker
executions admit a retained-result authority (including the exact explicit
current-HEAD destination where applicable); projectless executions admit
exactly `any`. Candidate evaluators use read-only or CoW-discard authority.
The one candidate-integration root uses retain-current-HEAD only to preserve D
for independent evaluation; it never advances HEAD itself. Only the later
owner-authorized publication root may perform the exact B-to-D HEAD CAS.

An owner may instead return a terminal retained candidate to a configured
source site with `service:worker-executions/candidate-result` and
`service:remote/pull-worker-result`. The target read service accepts only the
origin-bound configured owner and reconstructs current chain/placement, the
exact reserved command and turn-completion fence, separately bound launch and
persistent-session capsules, admitted project/base, and captured candidate.
Its v2 attestation describes a `frozen`/`retained` candidate, with fresh
closure/base evidence and its exact digest. This is not evaluator qualification
or publication authority. An interactive worker root stays running awaiting
disposition; a bounded worker root must already be authoritatively completed
with its exact candidate and terminal-outcome testimony. Returning it never
reopens that root or releases its finite-budget accounting. The source retains
the exact attestation and route in
an ordinary durable sync job, fetches the content-addressed closure, and
applies it through the existing clean-base atomic remote-result path. Exact
result-tree recognition closes retry after an apply/settlement crash. Pulling
does not advance either project HEAD and leaves the actual target disposition
unchanged (`retained` or `retained_for_review`); publication or discard remains
a separate explicit owner action.

For a runtime that declares native resume, a proved-dead launch owner does not
discard an unpublished CoW workspace. Startup retains the exact workspace
journal, verifies its backend, mount, and pinned root identities, and transfers
it only to the same thread's new launch claim. Immutable item/config resolution
is rebuilt from the admitted base snapshot in CAS; mutable workspace bytes are
not re-admitted as engine configuration. A crash during transfer is retryable
because owner replacement and stale process-attachment removal are one
transaction.

A terminal, detached placement with no unsettled worker boot may retain an
older session-capsule epoch as opaque history. Command-outbox startup replay
leaves those old facts and unknown outcomes unchanged rather than interpreting
them as the current protocol or preventing unrelated current sessions from
starting. This is not a compatibility decoder: old commands cannot acquire
current replay, completion-fence or resume authority. Malformed/current or
future capsules and unresolved active/cleanup authority still fail closed.

If restart occurs after candidate capture, startup first closes any interrupted
freezing workspace, repairs the missing root-fact-before-projection boundary,
and runs only the generic in-process disposition controller. It does not
restart or reattach the external worker to already-frozen mutable bytes. The
controller waits on pushed projection changes, reconstructs the canonical
generic session result after owner disposition, and commits the terminal root
event; candidate exposure is permitted only after workspace closure.

Shutdown settles a runtime process identity and its exact workspace membership
together, after proving group death. Process-only detachment is refused while
membership remains; unsettled descendants retain the parent's exact identity
for recovery. A missing identity cannot prove that an earlier borrower never
contacted the workspace.

An ownerless root with retained borrower membership or an uncommitted freezing
journal remains workspace-quarantined. Readiness recognizes that existing
durable authority without resuming the root or releasing its workspace or
credential fences. This is not successful execution recovery: unrelated node
work may proceed, but cleanup still requires exact settlement proof. An older
record that lost that proof is not repaired by interpreting NULL as process
death, and history is not implicitly reset.

## Child-execution observation

A hosted operator proves the tool operations a worker invoked through the
placement's settled command observation. For every workload-client dispatch
the daemon retains one runtime action intent binding the ingress provenance
to exactly one daemon-minted child execution; the command observation
projects each retained dispatch together with its child's authoritative
terminal snapshot and replayable result. The projection reads existing
state only: it never re-executes a child, never widens thread-children
listing authority, and fails closed when a child snapshot contradicts its
placement's ownership. Worker prose about child outcomes is orientation;
the projected dispatch, capsule, terminal, and result facts are the
acceptance evidence.

## Explicit non-claims

This substrate release does not provide hostile multi-principal containment,
provider-only egress, per-worker descendant quotas, worker pooling, invocation
leasing, cross-session reset, a scheduler, live migration, simultaneous active
placements, a workload-native remote-client gateway, or RyeOS local inference.

See also `knowledge:ryeos/core/kinds/worker` for the generic authored worker
kind.
