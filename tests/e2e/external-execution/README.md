# External candidate execution qualification

Status: source implementation in progress; **not a supported execution backend**.
No external profile is enabled, no allocator is connected, and these tests must
not be used as evidence that a remote worker can safely run.

## Protected connector capability provisioning checkpoint

The controller vault now has a separate app-private generation for the local
external-candidate connector capability. It is distinct from placement
credentials and the remote supervisor bootstrap, uses a domain-separated
reserved physical key, and is absent from operator and runtime-bundle reads,
listings, writes, and deletion. Creation is atomic and insert-only; concurrent
ensures select one exact capability and reopening returns that same generation.

The canonical schema-1 document binds one lowercase SHA-256 generation to one
random 256-bit capability. The plaintext is zeroizing and exposed only through
the crate-private connector-configuration accessor. Authentication derives the
hash from the decoded raw capability bytes and compares fixed-length canonical
hashes in constant time. Focused source evidence on 2026-09-21: all 4 connector
vault tests passed, covering concurrent creation, reopen, malformed coordinates,
schema/generation substitution, unknown fields, noncanonical JSON/base64,
failed-provision rollback, immutable replay, default-backend refusal, and
operator/runtime invisibility.

This checkpoint is protected secret provisioning only. It does not persist a
connector occurrence, open a listener, authenticate a peer, grant reconnect or
input-replay authority, configure Codex, or contact a provider.

## Durable one-use connector lifecycle checkpoint

Runtime operator epoch 55 adds a retained controller-local connector occurrence
after the exact authenticated supervisor Ready has been applied and its one
candidate Release has been authored. The record binds the placement, channel
digest, execution binding, signed protocol and artifact coordinates, derived
capability generation, and capability hash. Its only forward states are
`prepared -> connected -> closed` or `prepared -> closed`.

The first authenticated peer wins one SQLite compare-and-swap and retains its
canonical exact Lillux process-incarnation identity. An identical repeat returns
durable uncertainty rather than reconnect authority; a different peer and every
post-close connection are refused. Connection rechecks the live session owner,
workspace readiness, Running channel, sticky revocation, execution deadline,
and deterministic capability generation inside the writer transaction. Reopen
rederives the generation and every channel/artifact join, including closed
history. Capacity settlement is refused until the connector is closed, and the
row and authenticated peer evidence are immutable.

Focused source evidence on 2026-09-21: connector lifecycle tests cover release
gating, exact preparation replay, Prepared/Connected/Closed reopen, same-peer
uncertainty, changed-peer and post-close refusal, revocation/quiescence/stopping/
expiry/orphan races, retained-generation corruption, terminal closure, and
capacity settlement exclusion. The independent production-DDL suite covers the
one-way SQL transitions and passes 26 tests. This checkpoint does not yet open
the owner-private listener, verify the installed peer executable, relay protocol
bytes, generate Codex configuration, or contact a provider.

## Signed connector admission checkpoint

Runtime operator epoch 54, signed external-binding schema 6, external-candidate
requirement schema 3, and outer-supervisor journal epoch 4 make the controller
connector part of exact execution authority. The signed structured-session
requirement admits only `ryeos.external-candidate.connector.v1` with the closed
`connector_only` route; there is no local or automatic fallback spelling. The
node-signed placement generation independently binds that protocol to one exact
connector artifact hash and byte length.

At daemon composition, an installed companion executable is opened and pinned
beside the daemon. Admission joins the signed coordinates to that exact open
file and rechecks both pathname identity and stable bytes before reading a
placement credential or qualifying a provider adapter. Missing installation,
wrong hash, wrong size, pathname replacement, and same-inode byte mutation all
fail closed. The retained connector can also verify an authenticated Unix peer
against the exact executable name, size, and digest; the future start owner must
perform that peer check before authenticating a one-use connector capability.

The persisted-format cut is intentional. Epoch-53 runtime stores can contain
schema-5 placement generations without connector authority, and epoch-3 outer
supervisor journals can contain requirement-schema-2 bootstraps. Neither is
decoded or migrated as current authority. Existing explicit predecessor-reset
classification remains the only runtime-store transition; outer occurrence
journals remain historical/recovery evidence and cannot reopen under epoch 4.

Focused source evidence on 2026-09-21: the bounded daemon/app/API/executor check
passed; 4 state admission tests, all 15 structured-profile tests, all 19 signed
node-loader tests, 16 placement/admission tests, 13 external runtime-journal
tests, 7 outer-supervisor journal tests, and all 25 persistent-session tests
passed. Coverage includes invalid connector routes, exact signed artifact
coordinates, missing and drifted installed artifacts, pre-credential refusal,
retained-generation replay, and predecessor-epoch refusal. No external service,
provider/model, lifecycle API, credential, grant, installation, deployment,
release, or paid resource was contacted or changed.

This checkpoint is admission only. It does not yet mint the occurrence-private
connector capability, start or authenticate a connector peer, generate the
protected provider environment, enable an external session, or qualify the
complete B -> C -> evaluation -> D workflow.

## Protected supervisor executable checkpoint

Runtime operator epoch 53, signed external-binding schema 5, supervisor
bootstrap schema 4, and outer-supervisor journal epoch 3 bind the exact admitted
launcher artifact into placement, activation, durable launch intent, and guest
startup. The production supervisor is now an executable with a closed fixed-FD
boundary: its sealed bootstrap, state root, candidate runtime, candidate-private
parent, launcher, and runtime mount arrive only as inherited authorities. It
accepts no argv, ambient project path, provider credential, node grant, or
mutable launcher selection.

The supervisor independently verifies directory-tree disjointness, the exact
launcher bytes, and the exact admitted runtime tree before attachment or launch.
Its occurrence-private state anchor permits fresh construction only for an empty
state root. Reopen after durable launch intent returns typed recovery-only
evidence and cannot attach again or spawn another launcher. The bounded control
loop distinguishes retryable transport ambiguity from fatal local failure,
honors separate execution and post-capture deadlines, and reaps the exact native
launcher on every terminal path.

Executable-boundary coverage invokes the real binary through Lillux descriptor
mapping. It refuses missing, unsealed, oversized, and noncanonical bootstrap
documents, reaches the next fixed authority only after accepting canonical
bootstrap bytes, and proves a retained launch intent exits recovery-only without
controller contact or launcher execution. That test exposed and corrected a
generic Lillux mismatch: inherited directory authorities no longer attempt to
reopen diagnostic pathnames, while path-opened authorities retain their pathname
binding checks. Missing inherited fixed descriptors are validated before Rust
constructs an owning `File`, preventing an I/O-safety abort on refusal.

Focused source evidence on 2026-09-21: the real supervisor TLS/control suite
passed 19 tests with one subprocess-only helper ignored; the executable boundary
passed 6 tests; the Lillux inherited-directory group passed 8 tests; application
external execution passed 31 tests; placement lifecycle passed 8 tests; signed
node configuration passed 19 tests; shared state external execution passed 51
tests; and the independent production-DDL Python suite passed 25 tests. No
external service, provider/model, lifecycle API, credential, grant, install,
deployment, release, or paid resource was contacted or changed.

This checkpoint does not yet connect the daemon's dedicated-session start owner
to the external placement lifecycle, settle ambiguous start/cleanup through the
compound completion owner, install a provider adapter, or prove the complete
B -> C -> evaluation -> D workflow.

## Controller-owned durable candidate import checkpoint

Runtime operator epoch 52 makes the controller's signed frame transcript the
sole durable staging authority for imported candidate content. Export chunks
remain Retained until the exact seal and the guest's signed Applied evidence for
the matching owner Quiesce are both present. The controller then claims the
complete authenticated export prefix atomically, reconstructs it beneath one
pinned CAS/write guard, validates the exact B -> C closure and writer-exclusion
evidence, and commits the GC roots together with every export-frame Applied
transition. A transaction failure exposes neither roots nor Applied evidence.

The process-local import pool is only a bounded wake/coalescing owner. Duplicate
wakes cannot lose a prerequisite transition, competing seals for one placement
are refused, and daemon shutdown fences new work and waits for live owners.
Ingress discovers eligible work after every authenticated exchange because the
seal normally precedes the guest's later Quiesce acknowledgement. Startup and
periodic recovery discover the same pending or already-claimed seals from the
durable transcript. A crash may therefore repeat deterministic content-addressed
CAS writes, but never candidate execution. The next authenticated poll also
authors the upgraded owner-signed Applied seal receipt from rooted durable
facts; a prior Retained receipt or HTTP success cannot manufacture it.

Focused source evidence on 2026-09-21 covers seven import-owner tests and all 31
application external-execution tests. It includes exact-wake coalescing,
competing-seal refusal, shutdown fencing/timeout, ownership release after an
error, the production exchange-to-pool-to-StateStore path, seal-before-Quiesce-
ACK discovery, claimed-prefix reconstruction after reopening with an existing
partial CAS prefix, cancellation before versus after claim, malformed staged
content refusal, atomic rollback between root insertion and frame application,
exact replay of an already applied import, missing retained-content refusal on
that replay, and publication of the upgraded Applied receipt. The state
external-execution suite remains green at 51 tests. Independent architecture,
security/recovery and testing reviews found no remaining blocker in this
checkpoint. No external network, provider/model, lifecycle, credential, grant,
installation or paid resource was contacted or changed.

This checkpoint proves content retention only. It does not treat imported C as
worker completion, writer qualification beyond the retained native evidence,
evaluation, integration, publication, occurrence cleanup or capacity release.
The persistent-session completion owner and composed B -> C -> evaluation -> D
workflow remain outstanding.

## Cross-machine candidate export checkpoint

Runtime operator epoch 51, guest-journal epoch 8, outer-supervisor journal
epoch 2, and external channel/supervisor-bootstrap schema 3 separate the raw
candidate-content ceiling from the authenticated wire-byte ceiling. A pinned
guest CAS now derives the exact candidate object/blob closure itself, verifies
every complete member before emission, and emits deterministic bounded
`export_object_chunk` frames. It accepts no pathname, archive, peer manifest or
live project reconstruction. Empty blobs and members larger than one chunk
retain exact hashes, offsets and final-chunk boundaries.

All export chunks and the exact `export_sealed` terminator are authored in one
SQLite immediate transaction. A frame-count, encoded-byte, signing or storage
failure therefore retains no partial export prefix. Reconnect reads the earliest
unacknowledged canonical frame from the durable guest journal on every exchange;
a later locally generated acknowledgement cannot overtake export predecessors.
An ambiguous response retries the same chunk bytes and advances only after the
controller's signed cumulative acknowledgement.

Focused source evidence on 2026-09-21: 51 state external-execution tests and 13
executor transport tests passed with one Cargo job, including separate guest and
controller CAS stores, multi-chunk and empty-blob reconstruction, raw-budget
refusal before the first frame, atomic batch rollback, a real-journal ordered
chunk/seal stream, byte-identical retry after a lost response, and replay of a
delivered chunk until a delayed signed receipt advances the durable frontier.
The live-journal transport also crosses only an exact process-locally delivered
acknowledgement when the controller intentionally emits no acknowledgement of
that acknowledgement; the later candidate output remains ordered and makes
progress without weakening durable replay for data or export frames. No external
network, provider/model, lifecycle, credential, grant, installation or paid
resource was contacted or changed.

Independent architecture, durable ownership/recovery, and tests/documentation
reviews found no remaining blocker after the ordering, delayed-receipt, and
acknowledgement-of-acknowledgement regressions were corrected.

This checkpoint proves the guest export source and reconnectable outbound
transport. Controller-owned durable assembly/application, production lifecycle
attachment, executable/provider qualification, and the complete
B -> C -> evaluation -> D workflow remain unqualified.

## Durable external supervisor authority checkpoint

The protected external supervisor now has an occurrence-private rollback-delete
SQLite journal outside the candidate and guest store. Before first attachment it
durably retains the exact sealed bootstrap, a fresh supervisor signing key, and
the canonical attachment request. An ambiguous HTTPS response can therefore be
retried after process restart with byte-identical request and key material; it
cannot silently create a second channel identity.

An accepted binding is committed before candidate preparation. The supervisor
then reserves the exact empty guest database inode, retains that independent
anchor together with the exact launcher specification/bootstrap/artifact
digests, and commits one-way launch intent before native spawn. Reopen may
continue only the already-retained `prepared` or `attached` pre-launch
transition. Once launch intent exists, the recovered type exposes evidence and
validation only; it cannot return to attachment or launch authority. An
abandoned empty guest reservation similarly has no reopen path.

Both outer and guest stores hold an exclusive owner-private directory lock and
verify exact directory/database inode identity. Recovery validates the complete
SQLite table, constraint, index, and trigger SQL—not only application/version
markers and `quick_check`—plus every canonical bootstrap, request, key, binding,
guest anchor, and lifecycle join. Focused tests refuse removed/replaced triggers,
extra tables, same-directory database replacement, retained-content
substitution, guest binding/bootstrap substitution, nonempty or replaced guest
reservations, ambient entries, and competing live owners. They reopen every
pre-launch stage, including launch intent while the guest inode remains empty.
The native HTTPS regression drops and reopens the outer journal between a lost
attachment response and an exact retry after the original bootstrap deadline.

Focused source evidence on 2026-09-21: 6 outer-journal tests and 15 guest-journal
tests passed with one Cargo job. The durable HTTPS retry regression passed
outside the sandbox because it requires a loopback TLS listener; its initial
sandbox failure was only `EPERM` at local bind. No external network, provider,
model, lifecycle, credential, grant, installation, or paid resource was
contacted or changed. This checkpoint still does not provide the production
supervisor executable/control loop or complete remote worker workflow.

## Exact external candidate program checkpoint

Runtime operator epoch 50 and guest-journal epoch 7 cut the external execution
wire to requirement, supervisor-bootstrap, and channel-binding schema 2. The
signed profile now owns one closed credential-free runtime recipe: its exact
content-relative executable, arguments, working directory, environment, mount
destination, output limits, proc policy, process-group containment, and nested
sandbox requirement. The qualified runtime product continues to own the exact
runtime bytes and qualification evidence. Their joined admitted-program digest
is retained in the capsule, placement preparation, supervisor bootstrap,
attached channel, guest journal, and launcher bootstrap.

The dedicated launcher independently verifies that every projected launch
coordinate is identical to the admitted recipe. A 96 KiB preallocation recipe
ceiling reserves enough space for the duplicated program/projection plus the
fixed channel envelope inside the launcher's 256 KiB descriptor limit, so a
recipe cannot pass allocation admission and become unrepresentable only after
attachment. Placement recovery reloads the capsule and reconstructs the exact
private program projection; it does not accept a caller-supplied recipe or
reconstruct authority from provider state.

Focused source evidence on 2026-09-21: 41 state external-execution tests, 15
structured-profile tests, 8 placement-owner tests, 29 application channel tests,
19 node-config loader tests, the structured-session refusal regression, 25
independent production-DDL SQLite tests, 9 protected-supervisor TLS tests, and
the native inherited-descriptor launcher test passed. After review hardening,
the 4 admission tests and 4 launcher recipe/projection tests passed again, as
did the bounded seven-crate test check. Architecture, security/recovery, and
testing review found no remaining authority defect after adding the conservative
recipe ceiling and direct path/budget plus complete projection mutation tests.
No installation, lifecycle, provider/model, Render, credential, grant, or paid
resource was contacted or changed.

This historical checkpoint did not provide the production supervisor executable,
production lifecycle adapter, structured-session connector, or complete
B -> C -> evaluation -> D proof. The later cross-machine export checkpoint above
supersedes its object-transfer limitation.

## Bidirectional candidate transport checkpoint

Runtime operator epoch 49 and guest-journal epoch 6 add the missing candidate
stdout half of the protected protocol. The fixed inherited launcher protocol now
provides bounded output polling over the same descriptor-authenticated channel
used for input and capture. The launcher retains candidate stdout instead of
discarding it, performs an exact-bound EOF/overflow probe, and never exposes an
ambient path or descriptor.

The serialized supervisor signs non-empty candidate output as
`protocol_bytes` and records one distinct `protocol_eof` observation when the
endpoint closes. EOF is supervisor-only and preserves the running or quiescing
phase; it is not candidate success, writer exclusion, export retention,
occurrence cleanup, or capacity-release evidence. The transport driver polls
output only without a pending supervisor frame and retains the exact signed
frame across ambiguous HTTP failure instead of reading the candidate twice.

Transport completion is now typed. Only a controller-signed `applied`
acknowledgement naming the exact locally authored `export_sealed` frame yields
`ExportApplied`; retention, claim, an unrelated acknowledgement, EOF, HTTP
success, or an empty poll cannot manufacture success.

Focused source evidence on 2026-09-21: bounded test compilation passed for
`ryeos-state`, `ryeos-app`, `ryeos-executor`, and the external launcher;
40 state external-execution tests, 20 executor external-candidate tests
(including the real inherited-descriptor child), and 25 independent
production-DDL SQLite tests passed. The regressions cover supervisor-only EOF,
EOF lifecycle preservation, author-failure retention and one-shot EOF, exact-bound
EOF/idle/overflow, byte-identical ambiguous retry without a second read, duplex
progress during partial stdin, no post-capture polling, and exact sealed-export
application. The descriptor child required the explicit test sandbox exception.
No installation, lifecycle, provider/model, Render, credential, grant, or paid
resource was contacted or changed.

This checkpoint still does not provide the protected supervisor executable,
production lifecycle adapter, structured-session connector, or complete
B -> C -> evaluation -> D proof.

## Durable supervisor activation checkpoint

Runtime operator epoch 48 and external binding schema 4 separate occurrence
allocation from the one allowed supervisor-start mutation. The signed binding
retains the exact ordered TLS roots as well as their digest, runtime manifest
and runtime selection. Allocation no longer transports activation secrets.
After an occurrence is bound, the controller first retains a canonical
activation intent, then either performs that exact mutation once or reconciles
it after an ambiguous response or restart. The activation capability remains
non-cloneable, non-debuggable and zeroized on drop.

Recovery recomputes the activation-request identity from the retained binding,
reservation, occurrence and channel-authority hashes. First mutation rechecks
the exact live dedicated-session/workspace owner in its SQLite writer
transaction; exact retained replay remains reconciliation-only after owner
state changes. Channel registration joins the activation transactionally:
pending or positively observed activation may attach, while `not_started` and
attachment exclude one another in both commit orders. Activation and external
termination also share one process-local lifecycle gate, so cleanup cannot
settle while a delayed start mutation remains possible.

Focused source evidence on 2026-09-21: application test compilation passed;
29 runtime allocation/channel tests, 8 placement-owner tests, the exact signed
binding admission/rotation test, and 25 independent production-DDL SQLite tests
passed. Regressions cover orphaned first activation, exact restart replay,
canonical intent/observation digest replacement, both attachment/non-start race
orders, and activation/termination exclusion. Independent architecture,
reservation-owner/recovery, and testing re-review found no remaining blocker
after the specific recovery-validator assertion was corrected. No install, node lifecycle,
provider/model, Render, credential, grant, or paid-resource contact occurred.

This checkpoint still has no supervisor executable, production lifecycle
adapter, dispatch integration, or complete B -> C -> evaluation -> D proof.

## Current program admission slice

Profile schema 9 requires `external_candidate`: either explicit `null` for local
execution or a closed `ryeos.external-candidate.stdio.v1` requirement naming one
runtime product declaration. Capsule schema 14 retains the resolved program in
its authority projection and recomputes it from the signed profile and exact
retained product selections. Omission cannot select local execution. The selected
runtime requires an admitted qualification and the protocol's fixed claims in
the signed product relationship; bare manifest hashes are insufficient.

This is program identity only. It does not grant cloud lifecycle authority.
`reservation.binding_hash` and `channel.execution_binding_hash` retain their
meaning as the protected operator binding generation, separate from the program
digest. A node-owned binding and retained placement owner now join that
generation to the capsule, selected runtime, backend artifact, credential and
narrowed capacity/deadline/byte limits. Existing host-runtime process-scope
bindings do not supply cloud account or credential authority. Fresh admission,
recovered execution, pooled execution, exclusive launch and the bridge must
continue refusing external profiles until the protected connector and qualified
lifecycle adapter are supplied.
Historical capsule inspection remains distinct from permission to execute it.

## Protected placement-binding retention checkpoint

Node admission now retains the exact verified signed binding source, signer
fingerprint and verifying key used by the loader. Before an external allocation
can be reserved, the application journal atomically retains a private canonical
generation containing that source and its decoded binding document. Recovery
re-verifies the signature, joins the signed body to the retained document,
recomputes the binding and stable capacity-owner identities, and checks every
allocation's requested concurrency and timeout against the signed maximums.
Binding generations are immutable while their obligation-aware collection path
remains intentionally unavailable. A failed reservation rolls the generation
insert back with the session-owner transaction.

Runtime operator schema epoch 43 contains this exact retained-generation table,
its immutability triggers, and the schema-2 binding document carrying exact
backend artifact, region/plan, lifecycle policies/deadlines and byte budgets.
Epoch 42 is not decoded under that stronger authority shape. Focused evidence
on 2026-09-20: 17 application
allocation/channel tests, 19 signed node-config loader tests, 21 schema-filtered
application tests and 22 independent Python/SQLite tests passed. The corruption
case widens an otherwise canonical retained allocation after bypassing its SQL
transition trigger and proves startup validation still rejects it against the
signed binding limit.

The application placement owner now loads the authoritative born thread, exact
owning session capsule, retained product selections and workspace generation;
qualifies the backend artifact and protected canonical account credential;
reserves the joined authority; and turns the durable claim into either a
non-cloneable one-shot allocator-contact permit or recovery-only authority.
First contact still requires the binding to be currently installed and the
session/workspace to remain unreleased and launch-ready. Existing contacted or
uncertain obligations instead use the retained binding, credential and exact
adapter artifact generation even after rotation. Adapter generations are keyed
by backend plus artifact identity so an upgrade cannot strand cleanup.

This checkpoint remains offline placement admission, not provider readiness.
The production registry is empty and fail-closed; no cloud/model/worker
execution is enabled by this slice.

The generic lifecycle boundary now distinguishes provider contact from cleanup
settlement. Runtime epoch 44 adds immutable authoritative no-occurrence
evidence, immutable termination intent, and immutable terminal-occurrence
observation. `no_contact` remains possible only before the allocator claim.
After contact, capacity can settle only as `contacted_no_occurrence` from exact
adapter reconciliation, or as `terminated` after an exact occurrence, a
controller-derived termination request, and independent terminal observation.
SQL transition guards require the corresponding retained evidence before they
decrement the stable obligation counter. Timeout, 404, local process death,
request acknowledgement and caller-authored success remain insufficient.

The adapter contract keeps allocate, allocation reconciliation, termination and
terminal reconciliation separate. Only the non-cloneable contact permit can
reach allocate. A process-local contact gate serializes the durable claim with
that call: exact negative reconciliation cannot settle while a delayed original
create remains possible, and the gate cannot create another permit after
`contact_pending` is durable. Recovery authority has no conversion back into
that permit. Every prepared contact or reconciliation authority also retains a
non-acquirable lease on the controller's exact OS-backed operator lock. A
bounded Tokio shutdown therefore cannot release controller exclusion while an
uncancellable blocking allocator, observer or terminator call remains alive;
replacement admission waits for the actual last mutator to stop.
The deterministic fault backend proves a create accepted with its response lost
is reconciled to the same occurrence after database reopen with one create
mutation; the equivalent termination-response loss settles from terminal
observation after reopen with one termination mutation. The composed owner test
also drives the actual permit, reconciliation and cleanup methods. Termination
intent atomically quarantines execution and requires sticky channel revocation
when a channel exists. Settled channels reopen as immutable history without a
live session lock, while new frames and application claims remain refused.
Focused evidence: 20 allocation/channel tests, 6 placement-adapter tests, 9
operator-lock tests, 36 shared state/journal tests, and 24 independent
Python/SQLite tests pass. This
remains an inactive generic adapter contract:
the installed registry has no production backend and the executor's protected
connector still refuses launch.
Independent recovery review found no remaining cross-controller handoff defect
after the operator-lock lease was added; the review remains source-level and
does not qualify an installed provider adapter.

The supervisor/launcher implementation through `f08547e9f` retains native capture
under a durable occurrence receipt and reconciles exact quiesce/export replay.
At the start of this slice, 33 state external-execution tests and the composed
supervisor capture-replay test passed. The composed test uses a recording launcher;
it proves one capture and one release across duplicate quiesce, not a complete
native or remote worker. Current focused checks pass: program resolution and
state-level external capsule roundtrip/tampering (3), profile compiler (15),
executor persistent sessions (25), Codex/OpenCode bundle checks (32 + 5), and
allocation SQLite regressions (22). The capsule fixture is structurally valid;
it is not installed signed-source admission. Launcher regressions (5) and the
bridge's protected-connector refusal test (1) also pass. The bridge check is a
profile refusal test, not a running external connector qualification.

Architecture, security/recovery and test reviews found no remaining defect in
the program identity slice. Review added executor refusal on recovered/pool/
exclusive launch, capsule/profile/proof cross-checks, immediately preceding
schema refusal, and exact-size launcher regression cases. Still required before
activation: valid external fixtures through the real admission/recovery entry
points with provider/allocator contact counters; installed full qualification
closure; and the complete execution lifecycle. A helper-only refusal test does
not establish those entry-point guarantees. Changed source closures and worker
definitions were refreshed and development-signed; full binary/bundle manifest
refresh remains part of final qualification.

Launcher bootstrap validation now rejects path spelling aliases, process-string
NULs and aggregate canonical JSON exceeding the receiver's 256 KiB limit before
preparing a launch. This closes a sender/receiver admission mismatch; it does not
establish placement, runtime integrity or native execution qualification.

## Occurrence-authenticated channel attachment checkpoint

Runtime operator epoch 45 cuts allocation reservations to schema 2. Before the
only allocator contact, each reservation now retains an app-private sealed
channel-authority generation, the exact controller Ed25519 public key, and the
digest of one random 256-bit bootstrap capability. The controller private key
and capability are insert-only generations in the node vault: operator and
runtime-bundle enumeration, reads, mutation and deletion cannot address them.
The protected supervisor generates its own distinct signing key; its private key
never enters the controller vault or candidate inputs.

The signed `/external-execution/channel/attach` route is composed only by the
daemon with an occurrence-specific verifier. The verifier grants no general
scopes, authorized-key class, grant authority or node enrollment. The handler
requires the exact synthetic occurrence principal and re-authenticates the
capability against the bound allocation before registering the supervisor public
key. Ordinary or unverified service principals fail before state mutation.

First registration atomically rejoins the exact occurrence, request, session,
capsule, base, retained lifecycle binding, admitted runtime and both channel
keys. The bootstrap attachment deadline is recomputed from retained authority
inside the SQLite writer transaction, with the production clock sampled only
after acquiring that lock. Lock contention or slow capsule validation therefore
cannot carry pre-expiry authority across registration. Exact same-key replay
returns the original binding; a changed supervisor key or any other binding
coordinate is refused. Channel nonce and execution deadlines are never renewed
by replay.

Focused source evidence after review corrections: 21 allocation/channel tests,
6 placement-owner tests, 4 protected channel-vault tests, 35 API tests selected
by the `external_` filter (including all 4 attach/verifier tests), and 24
independent Python/SQLite tests passed. The vault
tests include concurrent creation, operator/runtime invisibility and reopen.
Independent architecture, security/recovery and test reviews found and corrected
both expiry races: retained authentication across expiry and a clock sample
before writer-lock acquisition.

This is an inactive bootstrap and public-binding checkpoint. It does **not**
qualify a network exchange, TLS/server identity, signed-frame delivery, guest
supervisor deployment, protected Codex execution, or a production lifecycle
adapter. The installed backend registry remains empty and no external or model
contact occurred.

## Authenticated exchange and reconnect checkpoint

Runtime operator epoch 46 and guest-journal epoch 5 add the application-level
exchange required above HTTP. The signed daemon-only
`/external-execution/channel/exchange` route is composed with the same
occurrence-only verifier as attachment; it grants neither an ordinary workload
scope nor node enrollment. A request carries one exact supervisor-signed frame
and the controller returns exact owner-signed backlog, a separately selected
urgent revocation frame when cancellation must overtake an ordinary gap, and
the retained channel frontiers. HTTP completion is transport evidence only and
never advances application state.

Application acknowledgements distinguish the exact target frame from the
cumulative peer receive frontier. Receiving a valid acknowledgement applies
that acknowledgement frame's own no-effect transition atomically, so an
acknowledgement cannot permanently block later input. Exact signed-frame retry
is idempotent; claimed input is never replayed as new work. Reopen validates
both signed transcript and retained application projections.

Cancellation is sticky but does not fabricate negative execution evidence. The
guest may revoke locally retained, provably unclaimed input and acknowledge a
terminal control frame out of band across a missing predecessor. The controller
cannot make that proof about already dispatched input: it retains the input as
uncertain, fences replay, and still accepts late exact claimed/applied evidence.
The urgent lane therefore delivers cancellation without either deadlocking
behind an ordinary gap or converting an ambiguous outcome into non-execution.
Neither cancellation nor a stopped observation proves provider cleanup.

Focused source evidence on 2026-09-21: 38 shared state/journal tests, 13
application-channel tests, 37 API tests selected by the `external_` filter, 24
independent Python/SQLite tests, and the composed supervisor capture/replay test
pass. Independent architecture, reservation-owner/security and
tests/documentation reviews found and corrected cumulative-frontier confusion,
acknowledgement-of-acknowledgement blocking, cancellation behind a transcript
gap, and false controller-side revocation. Re-review found no remaining blocker
in this inactive exchange slice.

This is still not an installed or live network qualification. The production
backend registry is empty, no guest supervisor network loop or TLS/server
identity has been qualified, and no provider, model, Render, or worker contact
occurred. The signed route/service definitions and source checks do not prove a
remote B -> C workflow.

## Guest transport-driver checkpoint

The daemon and future guest executable now share one closed attach/exchange wire
schema from `ryeos-state`; the API handler no longer carries a second private
interpretation of that contract. A generic executor-owned transport driver joins
one exact `ExecutionChannelBinding` to the live protected supervisor. It retries
ambiguous network outcomes with byte-identical signed frames, bounds response
frames and bytes, validates every signed envelope, gives sticky cancellation
priority over ordinary backlog, and resumes partial protocol writes without
starting again at byte zero. Application acknowledgement remains distinct from
HTTP success.

The driver retains the exact cancellation application state. A claimed or
otherwise uncertain cancellation remains `RevokedAwaitingCleanup` across later
acknowledgement-only or empty polls; it cannot become successful revocation
without new durable evidence. Cancellation blocks later candidate input but is
not provider cleanup, occurrence termination, or capacity-release evidence.
Sealed-export application also remains pending until controller-signed
destination retention evidence arrives; the source guest no longer self-applies
its own export.

Focused source evidence on 2026-09-21: all 5 executor transport-driver tests and
the shared state wire-contract test passed, `cargo check -p ryeos-api --tests`
passed, direct formatting checks for the changed Rust files passed, and
`git diff --check` passed. Independent architecture/protocol,
authority/security/recovery, and tests/documentation re-reviews found no
remaining blocker in this checkpoint after cancellation uncertainty and partial
continuation were corrected.

This remains an inactive pure transport checkpoint. It does not provide the
guest HTTP client/executable, a sealed endpoint/TLS identity, lifecycle-adapter
activation, controller-to-guest bootstrap delivery, or an installed B -> C
execution. No network, provider, allocator, model, Render, installation, or node
lifecycle contact occurred.

The sections below retain earlier checkpoint evidence. References there to
missing guest implementation or older schema versions describe those checkpoints.

## Allocation journal

Installed binding configuration: `.ai/node/external_execution/*.yaml` is an
app-root-only, current-node-signed section. It retains the exact signed source
and a closed runtime/backend/account/credential-generation/limit contract.
Binding identity changes with signed authority; credential and limit rotation
do not reset the node/backend/account capacity domain. All 26 node-config tests
pass, including real signed admission, forbidden bundle contribution, duplicate
filename identities, limit boundaries and program-coordinate mismatches.
The installed value is not an allocation permit. The exact generation is
retained atomically with its first reservation and reverified during recovery;
only the application owner can convert that reservation into a one-shot contact
permit after the full born-thread/session/capsule/workspace/backend/credential
join succeeds. The first-contact CAS transaction independently rechecks that
the exact owning workspace is still Ready, so a prepared permit cannot cross a
later Active/Orphaned transition.

Protected credentials (inactive placement integration): the existing sealed
NodeVault now has a separate placement domain addressed by an opaque app-created
owner/generation coordinate. Provisioning inserts immutable generations and
allows identical replay only. No deletion API or environment fallback exists.
Operator and runtime secret paths cannot address that domain; CLI listings hide
internal entries and mixed removal requests are rejected before mutation. The
14 core-tools vault tests and 46 app vault tests pass, including the CLI bypass,
generation reopen, malformed-coordinate and unsupported-backend regressions. Architecture
and security re-review found no remaining storage-slice blocker. This does not
qualify placement admission, credential provisioning authority, or cloud cleanup.
The returned secret is zeroizing, but existing sealed-store temporary buffers
are not claimed to provide comprehensive memory erasure.

Protected-owner wiring checkpoint: node-config admission retains the exact
verified signed source bytes and signer without reopening the source path.
All 18 loader tests pass, including signed-fixture admission after in-place
overwrite and atomic pathname replacement. Raw allocation reservation, contact
claim, occurrence binding and channel registration are app-private. The owner
now performs the independent capsule/products and installed-lifecycle join.
Its backend registry remains empty in production until an independently
qualified adapter is installed, so this source boundary cannot contact a cloud
provider yet.

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
Epoch 40 first introduced the stable external-obligation reset guard; epoch 41
adds shared-journal initial-state constraints, so old stores are not silently
reinterpreted under a changed SQLite contract. Epoch 42 retains exact signed
binding generations, and epoch 43 cuts those generations to the complete
schema-2 backend contract rather than decoding prior rows as stronger authority.
Schema mismatch is checked before decoding version-specific journal rows. The
independent stable reset guard refuses destructive history reset even when the
controller's own host lifetime has ended.

Contacted settlement is now represented, but not yet connected to a production
adapter or the compound candidate completion owner. A contacted allocation
cannot be released through elapsed TTL, local worker death, a caller-supplied
success flag, or a missing provider response. Do not enable live allocation
until the composed transport/capture/completion path and installed adapter
qualification exist.

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
- Guest supervisor network control loop over the authenticated exchange and
  exact runtime/bootstrap realization, including installed server identity.
- Wiring and qualification of native capture and candidate-only content transfer.
- Connect verified external termination/restart reconciliation to the compound
  completion owner and qualify it through an installed adapter.
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
