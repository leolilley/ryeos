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

The daemon-side listener transfer and READY wait are present. The verifier now
has a conditional receiver concurrent with START, but the current signed
scenario still selects the historical driver and cannot activate direct Codex.
The signed direct-Codex recipe and its exact environment, working-directory,
and guest bindings are not yet admitted. No joined native direct-target test
has exercised the new conditional path.
Peer liveness is checked only at observation points, not through the complete
root/commit fence. Consequently, current component tests and observation-v4
parsing are not an external-runtime qualification. Do not enable a claim until
a joined test proves exact pre-release ACK, app-server conversation, provider
contact accounting, whole-scope/writer settlement, frozen candidate, and the
negative cuts for wrong/lost ACK, verifier death, cancellation, restart, and
replay.

The conditional START receiver also needs a complete failure owner. A
root-authorized exact-attempt abort callback now claims cleanup-only retirement
before checked process/scope stop, and the verifier calls it on conversation
or locator failure. If the receiver fails after START, the verifier uses only
the acknowledged locator or exact RESUME point read to request that same
child's abort. The abort and natural-observation CAS are mutually
exclusive; focused `ryeos-app` tests pass for both outcome orderings. This is
not yet a complete failure proof: a failed receiver may leave START waiting
until its deadline. A proved, reaped owned-root wait now enters a separate
cleanup-only path before runtime-result decoding or fallback finalization. It
uses the exact released-attempt abort CAS and waits for a natural observation
that won the race; it does not invent a cancel/kill stop intent. Pre-release
or predecessor-owned attempts still refuse fallback finalization. This path
has a focused CAS test and compile checks, but lacks a joined native root-death
test. Relay and scripted-provider settlement now retain their
owners on the first timeout, attempt explicit bounded cancellation, and refuse
qualification after cancellation. Their final interrupting Drop joins remain
potentially unbounded, so this is not a bounded terminal proof. Merely dropping
a local handle or repeating START is not recovery evidence.

## Direct-target launch-profile cut

The existing resolver can promote `subject/bin/codex` as an exact
realization-member executable: the signed recipe must name the sealed subject
declaration and manifest, the relative member `bin/codex`, and the verified
member hash. That closes executable selection only. It does not make the
current scenario-driver recipe a direct-Codex recipe. The verifier still
rejects a direct recipe in its signed-parameter preflight, and the current
producer request supplies only `RYEOS_EXTERNAL_REALIZATIONS` and the private
workspace root as cwd.

Recipe v4 now parses bounded, path-free environment bindings and logical
prepared-directory IDs for cwd and environment. The runtime resolves signed
literal values, but refuses path-valued workspace and prepared-directory
environment bindings before reservation. A retained preparation owner must
supply pinned directories, descendant-stable namespace paths, and effective
launch evidence. The signed ID now has a fixed isolated-namespace destination
under `/ryeos/producer-prepared/`; deriving that path does not install a mount
or authorize opening a host directory. The signed
scenario-driver fixture moved to v4 with empty bindings; previously signed v3
recipes require republishing and are not silently accepted. This is a schema
cut, not a direct-target launch or qualification claim.

The old verifier-owned child had additional effective launch inputs:
`--strict-config -c check_for_update_on_startup=false app-server`, a pinned
Codex home containing the exact signed `config.toml` and rendered
`environments.toml`, `CODEX_HOME`/`HOME` pointing to that home, closed
`PATH`/locale values, and inherited authorities for the guest executable and
guest cwd. Direct target must reproduce those conditions from a signed,
bounded, generic preparation contract before release. The preparation owner
must retain exact private workspace and source/config authorities, expose
only path-free typed environment bindings, and join prepared content,
effective environment, mount/descriptor identities, and applied launch in
the observation. A callback-provided path, ambient home, or live project file
is not a substitute. Codex-specific template rendering and app-server
interpretation stay at the signed Codex verifier/product edge, not in the
generic scoped-producer core.

Existing session environment bindings are a useful pattern, but cannot be
reused unchanged: their validator excludes `HOME` and `PATH`, and their
runtime-view mount destination is not the historical private Codex home.
The private workspace is already a retained descriptor and can contain a
pre-staged child home; that permits structural preparation checks now. It
does **not** by itself prove the old guest executable/cwd descriptor handoff
or prevent pathname substitution of a mutable child directory. Qualification
must remain disabled until the launch/isolation contract supplies and tests
those exact authorities, including a joined native direct-target run.

The prepared-directory integration must use a separate producer authority,
not reinterpret session runtime-view mounts. One logical ID binds a canonical
workspace-relative descendant and its pinned directory descriptor; the fixed
producer destination derives only from that ID. Isolation must compare the
descriptor with the retained workspace descendant, reject duplicate or
overlapping destinations across mount classes, and commit the resulting mount
through the existing applied-launch plan. Several environment names may refer
to one ID. The private-workspace binding likewise needs a stable namespace
destination; `/proc/self/fd/N` in an environment value is not a descendant-
stable contract.

There is a separate CWD cut: isolation currently interprets the requested CWD
as a host path before mapping it into the namespace. A prepared CWD cannot be
implemented by merely putting `/ryeos/producer-prepared/<id>` in the request.
The descriptor-backed mapping and CWD visibility check must recognize the
retained prepared descendant and commit its effective destination. All signed
IDs must resolve before launch reservation; disabled or non-enforced isolation
must refuse these bindings. Mount identity alone does not attest prepared file
content, concurrent writers, or final frozen output.

The first generic authority cut is implemented: Lillux can open an existing
canonical directory descendant from a retained descriptor without creating or
chmodding it. RyeOS has a distinct producer-prepared authority that compares
its source descriptor with that exact workspace descendant. Isolation context
now carries these authorities explicitly and refuses a nonempty set until the
mount plan and CWD mapping are joined. This cut has a passing focused Lillux
test and cross-crate compile check; it is not a producer-launch acceptance test.
Before accepting a nonempty prepared set, plan admission must reject duplicate
IDs and overlapping destinations, and the Codex preparer must prove exact
configuration content and writer exclusion. Directory identity by itself does
not establish any of those claims.
