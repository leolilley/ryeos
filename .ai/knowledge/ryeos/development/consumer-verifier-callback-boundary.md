<!-- ryeos:signed:2026-10-01T02:25:15Z:ef34cca109c80957bdd7bde5e91e7bc06c4c2cc71a4a85d869ca590657904cd7:zSY7qafmxrotdJtbaqYsvWUDRpCXqvK+TwgtxUB4kWN7ZmnXRo3qwkJ+wf4ZlaoHdgJKJLc3poNlPwpx5l2dAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# Consumer verifier callback boundary

The `ryeos-codex-runtime-verifier` crate and `codex-runtime-verifier` binary interpret a finite credential-free diagnostic on
an already measured restored occurrence. It has a distinct signed callback
protocol, `protocol:ryeos/core/qualification_consumer_callback`, selected by
`tool:codex/qualification/consumer-runtime-verifier`. The Tool selects the
`--consumer-controller` entrypoint and the Codex bundle owns its static payload.
An ordinary Tool launch cannot mint the sealed qualification-purpose grant.

## Invocation and protected inputs

The signed Codex configuration v2 selects the scenario, qualification operation,
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
starting the diagnostic deadline does not renew it. These are source contracts,
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

The signed consumer request contract is current-only v2. It commits an opaque
session setup request and an opaque turn request, with a combined 8 KiB ceiling
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

## Remaining product semantic contract

No authored signed Q scenario yet selects that finite conversation, and no Q
fixture profile joins the actual app-server configuration to the signed
expectations. The ordinary external-authoring profile pins a real model
provider; a verifier may not override that accepted profile later by injecting
an endpoint or argv. Actual Q's model-speaking app-server runs in the local
trusted Worker; the guest runs isolated exec-server commands through the existing
connector. A fixture-specific signed profile can select controller loopback
before accepted-root admission, without granting guest networking.

The credential-free scripted peer currently belongs to the separate diagnostic.
Its existing script expects a successful candidate patch, whereas Q's workspace
is read-only with zero exports. Q requires a finite signed scenario that proves
read-only tool behavior and refusal, or a separately admitted private scratch
target. Do not change Q's workspace/export authority to reuse the diagnostic.
The ordinary authoring projection also omits observations needed to independently
establish exact Q output and request behavior; omitted events cannot be
reconstructed from hashes.

The next product cut must bind a finite credential-free fixture to actual Q,
retain the required bounded observations and interpret actual bytes against the
signed read-only expectations. Reuse the existing scripted peer and its bounded
Responses framing, with a Q-specific canary/lifecycle owner under the original
controller deadline. Retain it through Q's local/native/provider settlement;
cancel and join on uncertainty without success testimony. Peer logs are secondary
loopback evidence and do not identify the selected executable by themselves.
Do not synthesize a Codex wire transcript from projected events or infer Q
success from the later diagnostic. Until the actual contract and all its joins
exist, the controller remains diagnostic and consumer qualification publication
remains refused.

Carry the existing authenticated callback invocation through Q admission;
never manufacture an operator HandlerContext or use historical evidence as
fresh contact permission. Preserve the single checked Bundle generation and
all original acceptance gates. The earlier scoped-producer verifier remains
a separate authority lane, with its existing claim meaning.
