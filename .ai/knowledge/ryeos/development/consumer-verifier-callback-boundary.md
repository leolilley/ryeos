<!-- ryeos:signed:2026-10-01T13:09:46Z:c11964e0779484297c86e127009893434b06b63f8e415685d94ac08d669a8e10:9LX6h37F+We4qC4oCQGrYeQV2lvTxovKSX47Z6fLZS/GXhrFD9ArNWu0pPM2UXfo/NQUw3rQqbWqJjBeMNwOAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# Consumer verifier callback boundary

The `ryeos-codex-runtime-verifier` crate and `codex-runtime-verifier` binary interpret a finite credential-free diagnostic on
an already measured restored occurrence. It has a distinct signed callback
protocol, `protocol:ryeos/core/qualification_consumer_callback`, selected by
`tool:codex/qualification/consumer-runtime-verifier`. The Tool selects the
`--consumer-controller` entrypoint and the Codex bundle owns its static payload.
An ordinary Tool launch cannot mint the sealed qualification-purpose grant.

## Invocation and protected inputs

The signed Codex configuration v4 selects the scenario and a finite controller
duration. The actual qualification operation and prerequisite owner-measurement
attempt belong to the invocation, not those static signed parameters. The launch
request carries a typed selection; the daemon authenticates its observed owner
measurement, exact qualification, original live occurrence and current protected
source selection before root admission and rechecks before sealing. Current-only
purpose v6 commits that full invocation as a required nullable field. Nonconsumer
purposes retain null; unsupported consumer lanes acquire no authority.

The inputs callback accepts only the closed scenario selector. The daemon derives
operation and prerequisite IDs from the sealed purpose, and root, capsule,
subject, admitted use and scenario-source commitments from the live admitted
callback grant. The measurement observation digest comes from the canonical
inner owner-measurement observation. Later authentication joins qualification
operation explicitly as well as scenario/prerequisite, because the coordinate
does not contain the operation ID. Static parameter digest equality, checked
Bundle generation and original deadlines remain mandatory.

Author static signed recipe, scenario, policy and node profile before creating
the actual occurrence and measurement. Launch with those real retained selectors
without rewriting/reloading static policy afterward. This avoids changing the
selected Bundle generation to insert per-run IDs. The capsule hash still cannot
be a prelaunch parameter, because the capsule commits those parameters.

The daemon authenticates the resulting coordinate against the born root,
retained capsule, accepted launch reservation, sealed purpose and current signed
occurrence profile. It rechecks callback expiry/revocation before returning
inert record inputs. Reservation and contact independently recheck the original
unstopped launch owner and same-occurrence live prerequisite in their durable
transactions. No input response grants executable custody or provider contact.

The invoking verifier creates its private input record once. Recovery reopens
those exact bytes and only observes the retained attempt. A lost START response
does not authorize START replay. Provider settlement rejoins the existing exact
occurrence termination journal; it proves provider death, separately from native
namespace/process settlement and the enclosing verifier's process settlement.

## Evidence and claim limits

The execution projector returns opaque retained attempt and termination
references. The daemon corroborates their complete typed journal records,
bounded canonical CAS evidence, exact root/capsule/purpose and signed enclosing
process settlement. Historical readback reconstructs the same complete proof;
it requires no live callback bearer. It is not product-semantic interpretation.

The controller proposes a finite result only after actual Q semantics and the
separate independent diagnostic/provider settlement pass. The issuer then
corroborates Q and the enclosing verifier's successful terminal/process death.
Source wiring and successful exit alone cannot establish qualification.
No installed consumer qualification or production scenario has passed acceptance.

The START blocking owner now drives the existing Q lifecycle first, after
joining the supplied input to its authenticated retained authority. Q uses an
independently allocated occurrence; the already observed restored owner
measurement is its provenance, not a claim of the same runtime instance. After
Q's signed setup and turn, native settlement, local cleanup and provider terminal, START
re-authenticates the original callback and prepares the diagnostic contact
deadline. The enclosing signed verifier lifetime has continued throughout;
starting the diagnostic deadline does not renew it. Q startup, both Request
sends and turn-observation waits are capped by the immutable callback expiry
and the once-captured accepted WorkerExecution lifetime.
Checks after returned relay/read operations refuse late success. Cleanup and
attachment retain their own bounded lifelines after expiry so an already-owned
process is settled rather than abandoned. These are source contracts,
not evidence that the installed lifecycle has passed.

The actual invocation is retained as process-local operator or callback
identity. Scratch/allocation reservation, first allocation contact, supervisor
activation, channel Release, local spawn, held Worker release and one-use
Request contact each consume fresh admission. Callback revocation/expiry and
the exact unstopped launch claim are checked through short writer transactions;
no bearer-store gate is held across a complete launch or Request. Exact process
attachment, observations and cleanup remain available after expiry. A refused
held release aborts/reaps the already-created process and retains its identity
for retirement. Historical activation and Release records cannot grant a fresh
workload contact. Independent operator recovery keeps its distinct admission.

The signed verifier receives the actual Q transcript through protected OBSERVE.
The daemon joins the retained manifest to Q's exact historical command, request,
root, capsule, native/local settlement and provider terminal. A read selects only
an index in that manifest; it never grants arbitrary CAS access. Each chunk is
at most 1 MiB and the complete wire transcript is at most 112 MiB. The product
verifier independently checks chunk and stream digests, canonical bytes and the
signed request. These wire limits do not describe peak resident memory: chunks,
assembled bytes and parsed JSON may coexist.

Q has already settled before the independent diagnostic starts. After observing
that diagnostic, the controller settles its provider before downloading Q's
retained chunks. Historical Q readback does not require the diagnostic occurrence
to remain live. The enclosing verifier callback, exact unstopped launch owner,
root and original deadline must remain current throughout disclosure. A slow
read cannot defer that diagnostic cleanup. Publication follows independent
interpretation and all evidence joins, including enclosing root settlement.
Requiring a completed root before Q contact would create an ordering cycle.

## Finite session setup and turn

The signed consumer request contract is current-only v5. It requires explicit
nullable credential-profile, finite-fixture and controller-canary selectors.
It commits an opaque signed setup and turn template. The combined setup and
actual turn body remain within 8 KiB and have distinct canonical digests.
The daemon sends the exact setup and retained adapter-bound turn through one
attached exclusive Worker; null canary selection retains the literal turn body.
It does not interpret Codex response pointers:
the structured bridge binds the upstream session during setup and supplies that
same binding to the turn route.

One Q command row retains both stages. Setup must cross its one-use contact
boundary, return a bounded Final with one upstream-session binding observation,
and retain the original sender's successful return before turn contact becomes
available. Setup Delta/turn-state testimony is refused. Each contact separately
consumes current callback/operator admission. A missing or uncertain setup
response fences the conversation and grants neither replay nor turn permission.
Both sends use the original channel deadline; successful setup does not renew
it. A fresh turn admission refusal retires the attached owner through existing
cleanup, without repeating setup.

The schema 5 transcript manifest retains the exact setup Final, its digest, signed setup-request
digest and owner-send marker alongside the turn Delta/Final and completed-turn
fence. Pushed batches are the whole Q session's ordered observations, including
setup. Their receipt order does not establish that a turn request caused them;
the request-correlated Delta/Final and turn observation gates remain distinct.
Structural readback authenticates both signed request digests and both actual
Final byte sequences. It still grants no semantic success or qualification.

## Bounded native fixture custody

A selected signed scenario may admit one separate fixture at
`/ryeos/qualification-fixture`: one safe regular, nonexecutable leaf and at most
64 KiB of raw content. Surrounding signed policy, bootstrap and encoded transport
limits also apply. Null admits no fixture. Session program schema 2 selects it
only from the scenario in the sealed invocation and binds it in the program
digest. A callback, runtime profile or scratch path cannot replace that selection.

The native launcher creates fresh backing beneath its original pinned private
parent. It refuses overlapping namespace destinations and descriptor-rooted
backing-directory aliases with admitted mounts. The fixture and held sandbox
remain in one native owner, with the fixture included in held/applied mount
commitments. `/workspace` remains read-only for Q and ordinary candidate exports
remain closed.

After the authenticated completion command, only that sandbox's once-reaped
Lillux proof permits the one-shot fixture read. Capture validates the entire
directory, owner, mount, links, modes, byte bound and stable file identity. A
failed or lost capture does not permit another native read. The guest journal
joins these bytes to the original applied runtime and completion command, retains
exact metadata and blob CAS roots under a distinct Q occurrence receipt, then
commits its settlement row before acknowledging completion. Guest epoch 13
retains the exact admitted program and stage identity for cold verification.

The capture metadata includes the original capture-free native observation.
The signed schema 3 settlement wrapper carries a required-null pair of capture
reference and bounded canonical content. The metadata never references that
wrapper. Reference-only or content-only testimony refuses; a reference alone
does not establish controller custody. The CAS object has exactly one blob edge; program,
contract, native-observation and derived ProjectFile digests are commitments,
not extra CAS links. These facts describe immutable custody, not product
semantics or a completed five-claim qualification.

Channel binding schema 5 reserves eight frames and 128 KiB per direction inside
the existing signed ceilings for the complete Q finish exchange: one completion,
one protocol EOF, one native settlement and their exact first lifecycle receipts.
A supervisor may acknowledge the owner receipt once; duplicate polling receipts
remain ordinary traffic. Live and cold journals refuse a second Q EOF. The
separate cancellation reserve remains available. Preflight rejects insufficient
signed capacity before provider contact. These bounds are transport accounting,
not execution or claim authority.

The controller records its own immutable receiver-stage locator before writing
or protecting any CAS root. An empty stage abandoned before that locator may
expire; retries reopen the chosen original stage and cannot create another
permanent receipt. A chosen stage that has not yet reached retained custody
remains eligible for the existing maintenance cutoff. If it has expired or is
missing, exact-stage recovery refuses; a new stage cannot substitute for the
recorded ID. This boundary does not promise unlimited pending recovery. Once the
two-root receipt is retained, that maintenance path preserves it. Runtime epoch
125 joins placement, exact owner/program,
binding, completion, native settlement, capture reference and stage identity.
Bounded metadata/blob verification uses the original signed fixture and native
predecessor, outside the StateStore/RuntimeDb lock. The verified receiver row and
settlement frame application commit together before acknowledgement.

Normal provider termination, recovery/lifeline disarm, positive completed-scratch
replay and historical/issuer reads recheck that original stage and CAS bytes.
Historical callers reuse their existing CAS guard; they do not renew a callback
or require current execution contact. An absent normal-intent or readiness probe
remains negative without demanding a capture that never happened. Revocation
and cancellation cleanup remain independent of normal fixture evidence. SQL
readback alone is not authenticated CAS custody. These source joins and authored
tests do not establish installed execution, product semantics or a Render proof.

The transcript is published after original Worker retirement and exact native
settlement application, before relay shutdown, private-root removal and provider
termination. Both fixture and null-fixture publication require the original
Applied native frame, exact command channel and historical retained owner. The
source, bind and cold reader rederive those joins in their own existing database
transactions; they do not require cleanup to have finished first.

Manifest schema 5 carries a required-null capture reference; stream schema 3
carries the matching required-null content. The inert bounded decoder checks
metadata/reference identity, canonical bytes and size; it grants no native or
process authority. Native authentication and receiver custody remain daemon
checks. Object closure follows capture metadata and its blob, never commitment-only
ProjectFile hashes. The existing total 112 MiB transcript ceiling includes capture
content; no new protected reader or evidence service is introduced.

Publication and positive historical reads verify the original receiver stage and
exact CAS bytes under their existing CAS guard, even after channel revocation.
Cancellation cleanup retains its distinct exemption so failed capture cannot
prevent retirement. Original successful evidence retention and identical lost-ACK
readback may finish after callback expiry; they cannot authorize another turn,
capture, provider contact or issued claim. Missing or substituted original custody
refuses positive evidence. Full integrated publication, replay, expiry and CAS-loss
regressions remain an acceptance requirement; isolated source predicates do not
prove those paths.

## Actual production Q semantics and remaining joins

The accepted external-authoring Worker pins its production provider and
authentication configuration.
Replacing it with a credential-free fixture profile would measure a different
effective consumer. Preserve that production identity: the credential-free
scripted peer, successful patch and canary belong to the independently allocated
diagnostic occurrence. They do not attest Q denial or Q workspace writes.
Q retains its read-only workspace and zero exports.

The production profile now projects canonical complete parsed notifications
(`notification_json`, at most 64 KiB) for twelve explicit method families,
including completed command items and opt-in raw responses. These are actual
message bytes after canonical serialization, not original wire whitespace or
an exhaustive log of every notification. Overflow, schema failure and missing
required observations refuse rather than truncate or reconstruct evidence.
Existing execution arguments, provider, credentials, routes and lifecycle State
authority are preserved. Raw events still need an explicit signed setup opt-in.

Current-only consumer-verifier configuration v4 selects the finite signed Q
expectation `ryeos.codex.q_command.v3`. It requires a changed positive fixture,
exact expected final bytes, command, POSIX shell presentation, remote environment,
cwd and output. The three distinct direct calls are the fixture write, a read
attempt against the original controller canary, and an explicit local-route call.
Raw calls, results, completed command items and turn identity must correlate.
The read must fail with exit 73 and the exact denial output; the local call must
have the exact unknown-environment refusal and no command execution item.
Additional effects, missing or substituted observations, and uninterpreted
CodeMode calls refuse. Assistant prose cannot replace execution evidence.
Generic null-fixture transcript publication remains supported; this Codex v3
semantic checker requires the positive captured write.

## Original controller canary and signed adapter binding

The request's required-null `controller_canary` selection admits one finite
original-controller read/local-refusal contract. The signed provider declaration
uses `ryeos.external-execution.provider-declaration.v2` and explicitly selects
`ryeos.external-execution.qualification-request-binding.v1`. Unsupported adapters
refuse before allocation. Ordinary provider configuration rendering retains its
existing v1 wire protocol.

The SAME captured signed configuration adapter receives bounded canonical public
input over a sealed descriptor and returns bounded canonical original bytes.
The existing Codex adapter package owns the one prescribed text slot and shell
recipes. Generic State validates bounded data and exact byte commitments; it does
not parse Codex turn syntax or interpret shell commands. No credential, capability
or canary value enters the binding input. The invocation consumes the original
remaining qualification deadline, with no renewed clock.

The daemon retains durable creation intent before the first filesystem effect.
It creates a private 0600 file through its original state-root custody and retains
the original file observation. A read-only exclusion witness keeps the complete
original guest descriptors and SAME materialization leases alive through held
startup. Public paths or identities cannot reconstruct that custody.

Before connector preparation, held spawn/release or setup send, the daemon
immutably retains the actual adapter reply, exact adapter declaration/artifact
identity, typed input/output digests and signed-template/resolved-body digests.
The send and its observation sink use that actual body. Reselection reads the
retained binding; it does not invoke another adapter or adopt another canary.
The Codex checker independently reproduces the product mapping and compares the
adapter tuple against the authenticated process configuration.

After exact local process and connector settlement, the SAME occurrence cleanup
gate protects original canary readback through its durable terminal commit,
before credential release. Observation refusal still permits proven-safe cleanup.
The existing local-start journal has three append-once nullable columns for
intent, binding and terminal evidence. Cold reads check their canonical record
and original program, guest activation, adapter, command and process joins.
They never reopen a serialized pathname as live custody. Manifest v5 carries
required-null terminal canary evidence; publication and protected readback join
it to the original retained source.

These source contracts and synthetic interpreter fixtures do not establish an
actual model-generated three-call turn, hostile writer exclusion, callback
expiry, native containment, accepted five-claim qualification or Render readiness.
The independent credential-free diagnostic keeps its separate occurrence and
claim requirements. All five ordinary admission claims remain disabled.

## Production credential and launch context custody

The accepted environment and WorkerExecution determine credential requirements,
route set, effect ceiling, environment slots, session/turn routes and lifetime.
Q derives them under the same checked Bundle generation as the exact accepted
Worker and profile, retains the resulting contract and rechecks it on current
resolution. The scenario supplies the explicit credential-profile instance;
it cannot override launch policy. Current credential-bearing environments refuse
null selection. The profile ledger checks active state, principal, generation,
authority revision, home identity and lock owner. Workload-family and subject
projection labels remain signed-use declarations, not independent observations
of secret authentication bytes.

The original Q local-start journal acquires its credential lock and immutable
custody record atomically, after current callback/root authentication. It reuses
the existing credential profile/home and operation gates; it does not copy auth
or create a competing dedicated session. Operation waits consume the original
execution deadline and precede the occurrence gate. Historical transcript and
cleanup paths do not acquire that execution operation or renew its clock.
Spawn, held executable release and both setup/turn contact require the exact
active credential fence. Turn observation detects revocation/generation changes.

The actual credential home receives the accepted baseline and occurrence-private
connector configuration, and supplies the bridge's workload-home binding.
Its pinned descriptor is retained and checked disjoint from Q's workspace and
disposable state. Q cleanup removes only its disposable roots; it never deletes
that credential home. Workspace/route/effect bindings come from accepted data,
with environment-slot collisions refused.

Unreleased Q custody fences profile clearing/deletion and survives cold lock
reconciliation, including an uncertain borrow/spawn without attached process.
Revocation can invalidate fresh contact while leaving cleanup custody intact.
Release requires exact local process death/terminal evidence and closed connector,
or proof that no borrow could have spawned a child. Release and exact old-owner
lock clearing are atomic; a released row cannot clear a successor's lock.
Historical release uses the retained process identity without requiring a current
callback, active generation or original disposable-directory descriptors.
Launch/send/readiness failures attempt the same release and preserve uncertainty.
No-Create rollback releases safe credential custody before its terminal cleanup.
Runtime epoch 124 and required-null journal fields are current-only; there is no
predecessor decoder.

## Finite result and issuance joins

The independent controller implements two explicit claim meanings:
`qualification_worker_command` covers the actual signed Q setup/turn, exact
command, output and exit-status expectation; `independent_candidate_diagnostic`
covers the separate scripted protocol, canary, patch, native observations and
provider-terminal checks. Both checks remain mandatory. The signed policy may
select only the supported subset; unsupported allowances or requirements refuse
before START. Neither claim asserts credential exclusion by production Q.

The controller emits the existing product-qualification result with its subject
from the protected retained purpose, four exact remote-attempt/termination
references and the Q transcript manifest hash. It returns successfully before
the daemon can observe its own process settlement. The issuer authenticates that
later settlement independently; recovery death cannot replace successful exit.

Issuance and fresh current selection reconstruct the exact Q program from
retained signed Worker/profile bytes and the authenticated captured product.
Worker, environment, WorkerExecution, policy, producer recipe and independent
verifier source are checked within one Bundle generation. The historical Q
reader rejoins normal applied-runtime/native/local/provider settlement, exact
program, released credential profile/contract, process incarnation and signed
setup/turn digests. All manifest-owned bounded chunks pass the shared structural
decoder. The daemon authenticates bytes and execution; the signed product verifier
interprets Codex behavior.

Consumer evidence requires matching result and top-level Q manifest references
and the remote proof with its signed Worker scenario. Nonconsumer evidence
refuses a Q reference. Immutable publication/recovery retains the Q manifest and
chunks in its verified closure. Historical reads require neither an unexpired
callback nor an active current credential, and grant no fresh execution.

## Ordinary Worker admission remains separate

Ordinary external Worker admission still requires `bounded_candidate_capture`,
`candidate_only_execution`, `controller_credential_exclusion`,
`native_writer_exclusion` and `no_local_execution_fallback`. The finite two-claim
consumer workflow does not satisfy that admission. Its exact successful Q
command and settlement are not explicit local-fallback refusal or a production-Q
synthetic controller-canary denial. Its read-only workspace and zero exports do
not provide a positive Q capture. The separate independent diagnostic proves
its own negative probes and capture on a distinct occurrence; identical artifact
bytes do not move those facts to Q's runtime instance.

Keep the ordinary admission gate closed until its actual evidence obligations
are satisfied. Extend finite signed probes only with deliberate bounded meanings
and the existing execution/return authority. Never echo the five names into the
allowlist, probe real credential bytes, fabricate an export from a read-only
workspace, or relabel diagnostic testimony as production-Q exclusion.

These are reviewed source contracts, not installed qualification evidence.
There is not yet an admitted authored Q scenario with final captured-product
identities, credential selection and raw-event setup. The current product
relationship has no consumer qualification policy binding. The focused libraries/binaries type-check passed at `c41137ef5` before this
invocation correction. Validation of the combined correction and every required
operational acceptance gate remain open. This source preparation
authorizes no installation, paid execution or publication.

Carry the existing authenticated callback invocation through Q admission;
never manufacture an operator HandlerContext or use historical evidence as
fresh contact permission. Preserve the single checked Bundle generation and
all original acceptance gates. The earlier scoped-producer verifier remains
a separate authority lane, with its existing claim meaning.

## Original provider configuration input

The signed provider configuration adapter returns one current, closed response:
private rendered configuration plus a finite public command-environment projection.
Provider syntax and interpretation belong to the signed adapter; the daemon does
not parse Codex TOML or evaluate a policy language. The projection supports a sole
command-connector environment, matching default ID and include_local=false. It
contains no credential, capability, endpoint, inherited descriptor or HOME value.
The existing 1 MiB adapter stdout bound includes the entire response envelope.
Private response storage and owned encoded/stdout/stderr buffers zeroize on drop;
this is not a claim of erasure of every library serializer scratch allocation.

The connector owner hashes the exact rendered bytes supplied to the sealed memfd
and retains their byte length and admitted adapter hash/size, declaration and
relative destination. A public value alone grants no process or contact authority.
Q preparation borrows the original credential-home PinnedDirectory used by its
held Worker; the ordinary path cannot substitute a Q configuration observation.
The private home ID must match original live credential custody. Neither the home
ID nor its directory path enters the public transcript.

While the Worker is still held, its one-use attachment transaction binds that
opaque observation to the exact process, original execution binding and Q start.
Current credential checks borrow this SAME transaction; they do not open a nested
SQLite transaction or move freshness outside attachment. Missing/mismatched home,
placement, adapter/destination or channel refuses held release. Existing abort/reap
cleanup remains available. The bounded canonical public configuration JSON is a
required immutable field of the existing Q process row, not a new evidence service,
CAS object or root. Runtime epoch 124 owns this storage contract. Authenticated peer
and cold/historical process readers rejoin the original retained adapter binding.

Transcript manifest schema 4 requires that exact process observation. Structural
source/retained reading compares it to the terminal process, alongside original
setup/turn, process incarnation and channel. Stream schema 3 adds bounded fixture
content; original protected chunk readback carries the public configuration
observation and captured fixture bytes, never private configuration or HOME. A
lost ACK cannot authorize rerendering configuration,
selecting a replacement HOME/process or renewing original contact.

Pinned Codex omits environment_id from a sole environment's advertised command
schema. Its checker accepts that omitted field only after the authenticated launch
projection proves the sole default matches the signed expectation and the existing
protected setup/turn selection. Explicit changed/local IDs and other argument
substitutions still refuse. This finite correction is not general defaulting and
does not change the claim allowance.

This evidence describes daemon-owned launch INPUTS and their held-process and
connector association. It does not prove independent configuration consumption or
close the merged HOME/system/project/managed configuration, extension/model inputs,
or complete effective tool inventory. Image generation and goals can create local
controller effects in the pinned defaults; an empty MCP table does not erase lower
configuration layers. The external author's own baseline
`external-authoring.config.toml` and immutable profile argv explicitly set
`features.image_generation=false` and `features.goals=false`. Its ordinary authoring
counterpart retains its separately chosen shared baseline. These two prohibitions
close named conditional families in the pinned source; they do not prove absence of
inherited MCP or other ambient configuration. External-product controls and a bounded
effect-family/source proof must close these separate inputs. All five hosted
Worker definitions bind the complete shared source closure, so a changed external
profile or added baseline also changes their source identities. Refresh and sign
every affected binding; existing captured Worker/profile evidence cannot stand in
for this new closure. The ordinary authoring product retains its own explicitly
chosen semantics. All five ordinary runtime claims stay
disabled until actual required evidence exists; three probes or a routing projection
cannot replace it. One Q turn also does not promise a hard internal model request,
token or USD cap. Unknown post-contact charges remain unknown.


## Finite original configuration inputs

Structured-session profile 11 requires a closed `configuration_absence` inventory.
Its three sorted, unique lists select absolute paths, startup-workspace ancestor
members and route-workspace ancestor members. Their combined bound is 16 entries;
original and expanded paths are at most 4096 bytes. Expansion includes filesystem
root, refuses depth above 64 directories and more than 1024 unique inspections,
and finishes before inspecting anything. Explicit empty inventories preserve the
other products' selected semantics; missing predecessor fields are refused.
Persistent-session capsule 21 retains the exact inventory independently and checks
its equality with the original compiled contract. No new database table or owner
is introduced. Existing absolute runtime configuration files cannot contradict
an absence candidate at the same path or an ancestor/descendant path.

The actual structured workload owner checks this inventory before starting its
subprocess. Startup cwd is the original controller B; route cwd is separately
selected by the signed external recipe. Existing Lillux no-follow inspection
accepts only missing entries/parents and refuses files, directories, links,
special entries, unsafe ancestors and inspection errors. It neither reads ambient
secret contents nor deletes host files. Existing private HOME, baseline, exclusive
credential-operation and descriptor custody remain separate. This observes the
existing trusted host/controller boundary; it is not protection against later
privileged host mutation or proof of absence of every upstream file read.

The external Codex profile selects Linux system/managed/requirements paths and
`.codex/config.toml` along both cwd ancestry chains. Its existing signed initializer
then calls `configRequirements/read` with literal null params and requires exactly
`{"requirements":null}`. It calls `config/read` with local cwd `/workspace` and
`includeLayers:true`, validating the finite merged effects and every raw layer,
including disabled layers, through the existing signed JSON-schema mechanism.
Raw session flags match the original signed argv; the HOME layer matches the
selected baseline plus a bounded trust-only project mutation. Other admitted
layers must be empty. Missing/unknown inputs, profiles, managed requirements,
inherited MCP and lossy tools projections refuse. The pinned merged serializer's
seven loaded feature injections and lowercase environment-filter keys are distinct
from raw TOML layers. Refusal errors expose only schema identity and instance digest.
Both start and resume reject non-null permissions/runtime-workspace substitution.

The sealed provider adapter input remains a separate descriptor-linked
`environments.toml`, selected through the original process/launch/peer evidence.
It is not an extra argv override or witnessed by `config/read`. Noise environment
overrides remain excluded by the admitted environment and cleared inheritance.
Pinned linked-worktree config lookup imports hooks from outside these ancestry
chains; the signed hooks prohibition and nonempty-layer refusal cover that effect
without claiming every upstream read is absent. `code_mode.enabled=false` sets a
default; model metadata may still select CodeMode, so the complete conditional
and nested tool-family source proof remains required.

Source-derived serializer fixtures and actual validator regressions are separate
from evidence that a live process loaded these values. The inventory and RPC
checks do not alone witness accepted-thread configuration, a tool registry, Q
canary denial or local-fallback refusal. Original custody/setup/turn/native capture
joins and all operational acceptance gates remain necessary. All five ordinary
runtime claims stay disabled until their actual required evidence exists. Any
changed profile or schema refreshes and re-signs every Worker binding to the shared
source directory; older retained evidence cannot substitute for that closure.
