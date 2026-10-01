<!-- ryeos:signed:2026-10-01T03:42:12Z:6a17ebc711e9b754a88ec7b04414fef892bc0227b4f45be73582f812dcaa8923:k3hJu/YRDNmTfj1gBwuOEMNepdH/Vk79PormP7wDU0nasJndj/azqg75CzUFoiNccO9ScKcJWGuAS7MWjFd5Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# Consumer verifier callback boundary

The `ryeos-codex-runtime-verifier` crate and `codex-runtime-verifier` binary interpret a finite credential-free diagnostic on
an already measured restored occurrence. It has a distinct signed callback
protocol, `protocol:ryeos/core/qualification_consumer_callback`, selected by
`tool:codex/qualification/consumer-runtime-verifier`. The Tool selects the
`--consumer-controller` entrypoint and the Codex bundle owns its static payload.
An ordinary Tool launch cannot mint the sealed qualification-purpose grant.

## Invocation and protected inputs

The signed Codex configuration v3 selects the scenario, qualification operation,
prerequisite owner-measurement attempt and a finite controller duration. The
inputs callback accepts these bounded selectors. The daemon derives root,
capsule, purpose, subject, admitted use and scenario-source commitments from
the live admitted callback grant. It derives the measurement observation digest
from the canonical inner owner-measurement observation. The capsule hash cannot
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

The controller currently emits diagnostic references and exits with an explicit
refusal. Consumer-context qualification publication remains disabled. Source
wiring, protocol checks and an independent diagnostic do not establish that the
actual qualification Worker, Q, followed its required conversation.

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

The signed consumer request contract is current-only v3. It requires an explicit
nullable credential-profile selector and commits an opaque session setup request
and an opaque turn request, with a combined 8 KiB ceiling
and distinct canonical digests. The daemon sends those exact two values through
one attached exclusive Worker. It does not interpret Codex response pointers:
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

The v2 transcript retains the exact setup Final, its digest, signed setup-request
digest and owner-send marker alongside the turn Delta/Final and completed-turn
fence. Pushed batches are the whole Q session's ordered observations, including
setup. Their receipt order does not establish that a turn request caused them;
the request-correlated Delta/Final and turn observation gates remain distinct.
Structural readback authenticates both signed request digests and both actual
Final byte sequences. It still grants no semantic success or qualification.

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

Current-only consumer-verifier configuration v3 requires a finite signed Q
command expectation: remote environment, command, command presentation, cwd,
output and successful exit code. The product interpreter binds the actual setup
thread and turn to the retained request, lifecycle observations and manifest;
requires one raw remote exec_command, its terminal result and the corresponding
completed command item before successful turn completion; and refuses swapped,
missing or extra effectful calls. Call/item IDs are Codex invocation IDs, not
guest process IDs. Both raw and item output must match the signed expectation;
assistant prose cannot replace command evidence. Interpretation grants neither
provider permission nor a qualification result.

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
Runtime epoch 121 and required-null journal fields are current-only; there is no
predecessor decoder.

These are reviewed source contracts, not installed launch/recovery evidence.
There is not yet an admitted authored Q scenario with final captured product
identities, credential selection and raw-event setup. Complete the full
Q/diagnostic/native/provider/enclosing-root issuance and retained current-selection
joins, then exercise the actual candidate. Until those joins and operational
evidence exist, the controller emits diagnostic references and explicitly refuses;
consumer-context publication and qualification claims remain disabled. This
source preparation authorizes no live model execution.

Carry the existing authenticated callback invocation through Q admission;
never manufacture an operator HandlerContext or use historical evidence as
fresh contact permission. Preserve the single checked Bundle generation and
all original acceptance gates. The earlier scoped-producer verifier remains
a separate authority lane, with its existing claim meaning.
