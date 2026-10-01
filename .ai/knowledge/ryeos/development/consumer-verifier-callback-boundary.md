<!-- ryeos:signed:2026-10-01T01:17:03Z:79e4ef8ad9a83da8acf8defdfb5540952eca55a528a9e1caa3fcda336ce74cd9:t+9D8IJybOvYbw6BxEi5FcnQd8BanRrMU+ohfYMc3V9bd057NN7DJwzOD4SD63RZRPGjJ2NsNQCMIeuUEDi4Cg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# Consumer verifier callback boundary

The Codex consumer verifier interprets a finite credential-free diagnostic on
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

The Q integration must keep the admitted verifier running while its existing
Q owner completes the Worker turn, retains the exact transcript and settles
native, controller and Q allocation custody. The signed verifier must interpret
that authenticated transcript before the restored diagnostic occurrence is
terminated and the verifier root completes. Publication follows all evidence
joins. Requiring a completed root before Q contact while settling its restored
prerequisite during the running callback creates an ordering cycle.

Carry the existing authenticated callback invocation through Q admission;
never manufacture an operator HandlerContext or use historical evidence as
fresh contact permission. Preserve the single checked Bundle generation and
all original acceptance gates. The earlier scoped-producer verifier remains
a separate authority lane, with its existing claim meaning.
