# Scoped Codex qualification boundary

## Decision and claim

The independent Codex verifier is a signed interpreter of the Codex app-server
protocol. It is not the authority for the identity or containment of the
executable it examines. For the strong external-runtime qualification product,
the exact admitted Codex realization member is the daemon-owned scoped target.
The daemon must corroborate its executable, applied launch, process identity,
whole-scope settlement, writer exclusion, and frozen output. The verifier must
corroborate the effective environment, scripted provider exchange, conversation,
and expected candidate behavior. A clean claim requires the join of both sets
of evidence; neither set alone qualifies the product.

This is a product-level trust choice, not a rule that every worker or verifier
must use direct target launch. A signed verifier-owned child can test protocol
behavior, and the daemon can attest its enclosing scope, but that combination
does not independently attest the exact child executable or its applied launch.
It must not be promoted to the stronger qualification claim. Conversely, this
decision does not add a second Codex worker workflow: local hardened, trusted
disposable, and external placement remain endpoints of the existing bounded
turn, candidate, evaluation, integration, and publication lifecycle.

## Why existing child paths do not establish this claim

Dispatch, follow, and detach create independently admitted RyeOS work items
with their own thread, result, and recovery contracts. They do not supply the
bounded interactive stdin/stdout ownership needed to drive one Codex app-server
session as a subordinate operation of the verifier. The existing Lillux child
primitive supplies interactive pipes, but leaves exact-child launch testimony
with the signed verifier. The scoped producer adds that daemon-owned launch and
I/O evidence without inventing another Worker, scheduler, or candidate owner.
This is not a general prohibition on children or nested execution.

## Release-before-provider gate

The producer must remain held while the daemon attaches the exact process to
the root-owned scope. For the credential-free scripted provider, the isolated
Codex target cannot reach the verifier's host loopback namespace. The signed
recipe therefore selects a fixed loopback ingress. Lillux transfers that exact
listener over a root-bound inherited channel. The verifier validates the
canonical handoff against its preflight source and ingress, starts its bounded
scripted relay, and returns an exact READY acknowledgment. Only then may the
daemon release the held target. An absent, wrong, stale, or ambiguous ACK is a
checked abort, never permission to release or retry bytes blindly.

The listener transfer proves local socket facts, not target namespace,
containment, or qualification. Post-release applied-launch evidence, process
identity, root/scope settlement, provider transcript, and frozen candidate must
still join at the terminal fence. A point-in-time peer-liveness peek is not a
lease: observation and publication must fail closed if the verifier disappears
or the enclosing root terminates before the evidence join is committed.

## Current implementation gate

The daemon-side listener transfer and READY wait are present, but the verifier
does not yet receive the listener or send READY; its historical scenario driver
still binds a listener itself. The signed direct-Codex recipe and its exact
environment, working-directory, and guest bindings are not yet admitted.
Peer liveness is checked only at observation points, not through the complete
root/commit fence. Consequently, current component tests and observation-v4
parsing are not an external-runtime qualification. Do not enable a claim until
a joined test proves exact pre-release ACK, app-server conversation, provider
contact accounting, whole-scope/writer settlement, frozen candidate, and the
negative cuts for wrong/lost ACK, verifier death, cancellation, restart, and
replay.
