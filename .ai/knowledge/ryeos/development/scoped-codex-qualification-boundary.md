<!-- ryeos:signed:2026-09-29T17:06:12Z:1fe632cd0c2bdca6490229e665ce3e4cfb3bfd01e23387023c27f846514a3974:xMXbGIW+LaDud8aay8S69y7UsRpOZdVutZND6BdAUoRuZvUXWBK43QGjmDccY6laHqg24ju3M/pP9tgYcD+1Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

## Render external-placement boundary

The hard scoped direct-Codex fixture is **not** a prerequisite for placing a
Farm worker in a Render sandbox. The Render hosted-workflow controller has
unconfigured process scopes and an explicit trusted process-group lane; it
cannot issue a `scope_empty` claim. In external placement, the controller
retains provider credentials and campaign authority while model-controlled
commands and candidate files execute in the disposable guest. The guest's
isolation, exact launch, containment, termination, export and authenticated
return need their own installed evidence. A controller process-group absence
is not evidence of guest containment, and the guest's isolation is not a
scope-empty claim about the controller.

The external-authoring worker's `guest-runtime` product relationship remains
disabled until an independent qualification policy and complete guest-runtime
evidence admit it. That policy may require the controller's trusted settlement
authority only when its claims do not promise local hard containment; it must
still prove the Render guest boundary and reject before provider contact if
the selected product cannot provide it. The hard direct-Codex experiment
remains a separate, fail-closed RyeOS qualification path. A scope-qualified
same-node controller or cross-node testimony contract is needed only to
activate that stronger product on a node without local scope authority, not
to make Render-hosted Farm workers possible. Do not copy its node-bound
testimony to a different controller or silently downgrade its claims.

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

## Signed Codex routed-runtime profiles

Source implementation update, 2026-09-29: the specialized checker is now
`crates/tools/codex-runtime-verifier/`, Cargo package
`ryeos-codex-runtime-verifier`, installed binary `codex-runtime-verifier`.
Its protocol interpretation remains specific to Codex app-server. Shared
qualification admission and evidence joins remain in their existing owners.
This source migration is not evidence of an installed checker, accepted
qualification, or Render activation. Direct and scenario-driver paths continue
to refuse claims while their evidence gates are incomplete.

The complete parameters retain the operation discriminator
`scenario: test.independent_routed_runtime.v1`, the existing exact content and
launch expectations, and `external_candidate_qualification_context`. Their
`configuration` requires a v2 profile:

```yaml
profile:
  schema: ryeos.codex_routed_runtime_profile.v2
  producer_scenario_id: native_codex_empty_candidate
  command_fixture: workspace_rg_version_require_absent_candidate
  candidate:
    relative_path: candidate-alternative.txt
    content: |
      signed alternative candidate
      second independently authored line
expected_producer_recipe_ref: config:fixtures/codex-runtime/direct-codex-empty-candidate
# Existing expected_producer_recipe and remaining configuration fields required.
```

The profile and candidate object are closed. Unsupported schema/fixture values,
extra fields, missing candidate data and predecessor profiles refuse. Scenario
IDs use the existing qualification policy name grammar (1–64 lowercase ASCII
letters, digits, `_` or `-`). The recipe reference is a canonical unsuffixed
Config ref. The daemon must admit the selected scenario and exact recipe under
the signed policy; the verifier compares the returned source ref and recipe
digest with its signed expectation and retains recipe-generation, locator and
relay joins.

Candidate filename and content are authored data. The filename is one ASCII
leaf of 1–64 bytes, starts with an alphanumeric character and otherwise allows
only alphanumerics, dot, underscore and hyphen. Content is 1–1024 UTF-8 bytes,
LF terminated, with no control characters except LF. The checker renders one
Add File patch, prefixing every content line with `+`, including blank lines
and text resembling patch delimiters. Candidate absolute path and URI derive
from the fixed `/workspace` coordinate and this validated leaf. No parameter
accepts a shell program or arbitrary patch operation.

The source-owned generator remains
`tests/e2e/external-execution/support/independent_verifier_scenario.rs`.
It authors the Tool, policy and recipe from independently admitted inputs,
under `fixtures/codex-runtime`. Generated Tool/policy versions are `3.0.0`.
The fixture bundle is `codex-runtime-fixture`; its diagnostic binary selection
uses `RYEOS_TEST_CODEX_RUNTIME_VERIFIER_BIN`.

| Selection | Baseline | Fresh candidate precondition |
| --- | --- | --- |
| Scenario key | `native_codex` | `native_codex_empty_candidate` |
| Recipe Config name under `fixtures/codex-runtime` | `direct-codex` | `direct-codex-empty-candidate` |
| Command fixture | `workspace_rg_version` | `workspace_rg_version_require_absent_candidate` |
| Candidate leaf | `candidate-strategy.txt` | `candidate-alternative.txt` |
| Candidate content | One authored line | Two independently authored lines |
| Exact expected output | Authored `/workspace` and selected tools product's ripgrep version output | Same exact bytes followed by `candidate-absent\n` |

The baseline command is `pwd; rg --version`. The second operation renders the
following command from the selected safe filename:

```sh
pwd; rg --version; if test -e candidate-alternative.txt || test -L candidate-alternative.txt; then exit 74; else printf "%s\n" candidate-absent; fi
```

It refuses an existing candidate, including a dangling symlink. Both profiles
retain the five mandatory responses: local-command refusal, routed command,
candidate patch, controller-canary read denial and terminal message. Framing,
correlation, exit meanings and containment/settlement checks remain compiled
Codex semantics. The command enum selects supported finite behavior; exact
output and candidate content are pre-authored signed expectations, never
learned from the subject. Source tests also hold the command enum and recipe
fixed while changing the signed filename and multiline content, so fixture
data variation does not depend on choosing another compiled command case.

The existing policy parameter digest covers all profile expectations. The
scripted peer, Codex observation checker, guest transcript/capture checker and
post-settlement frozen-file inspection consume the same authored candidate.
They require exact commands, paths and content, while the scripted-peer output
marker remains secondary evidence. Swapped filename/content must fail signed
parameter validation and the applicable observation or frozen-file checks.
This source migration enables no claims and preserves all five claim meanings.

Production runtime membership belongs to independently admitted Worker,
environment and product realization sets. The closure validator compares their
complete union, exact search/process environment and bound context digest;
requires pinned runtime mounts and existing safe-path validation; and checks
unique provider File, command-tools Tree and runtime-product Tree roles against
their bound manifest identities and proper admitted owners. Helper names and
mounts are no longer a compiled seven-member inventory. A changed supplied
closure still refuses unless it equals those independently admitted sets.
Provider realization selection follows the bound manifest and the exact
admitted profile's workload ID. The fixed four checker inputs, subject and
relay entrypoints, guest `/runtime` and `/workspace`, and authoring-tools shell
mount remain the current versioned probe ABI.

The production closure helper currently has test-only callers. Its source tests
exercise admitted renamed/expanded inventories and substitution refusals;
they do not establish operational wiring of the consumer-context evidence
join. That remains an external source dependency of the ongoing Codex baseline
work, alongside the checker's outstanding collector/provenance joins. Both
owners must retain their current claim refusals until those checks exist.

This is a coherent checker/definition migration with no defaults or old-name
alias. New checker and generated Tool/policy definitions must be admitted as
one generation; retained predecessor evidence keeps its original identities
and cannot be reinterpreted. The generic qualification policy schema remains
v2; its existing parameter object carries the change. The complete definition
still must satisfy the 8 KiB checker-input and 16 KiB policy ceilings.

| Change | Required work |
| --- | --- |
| Change supported candidate filename/content, exact expectation or scenario/recipe selection | Sign new definitions, admit exact identities and requalify affected evidence; the checker binary and unchanged subject bytes need no rebuild |
| Change admitted Worker helper membership | Update and admit the owning definitions; require exact closure/role validation and affected qualification evidence |
| Add another finite command/protocol operation or change the fixed probe ABI | Review and rebuild the checker and affected producers/consumers, update signed definitions and qualifications |
| Change execution or evidence authority | Review its existing owner, compatibility and qualification requirements; profile data cannot grant authority |

Local source, signed-fixture, synthetic observation and pinned-file checks
establish only their stated boundaries. Actual checker runs with the same
artifact and complete joined strong qualification remain separate acceptance
requirements. Profile changes must never refresh an already admitted running
root's selection.

## Current implementation gate

The currently generated direct-Codex fixture is a diagnostic product check.
Its signed qualification policy has no `consumer_execution_context`, and the
Codex bundle's `guest-runtime` Worker relationship still has
`policy_ref: null`. Its five finite behavior claims therefore cannot be used
as proof that the production Worker received or ran the same runtime in a
hosted guest. A separate signed consumer policy and relationship must bind
the admitted Worker, environment, bounded-turn execution, exact product and
hosted lifecycle before that stronger claim can be selected. The draft v10
consumer evidence coordinates in the X1 source worktree are structural only;
the daemon does not issue them, and issuance, claim validation and fresh
selection remain closed. Do not promote that structure or a direct diagnostic
run into Worker activation evidence.

Retained-observation source cut, 2026-09-30: the daemon read path now compares
the post-release applied executable-path, argv, environment and CWD commitments
with the pre-release target digest in the exact relay handoff. A qualification
scoped-attempt proof also requires that handoff's scenario, signed recipe
source and loopback ingress to match the admitted direct producer recipe.
`ryeos-app` release-mode library and test-source checks pass for this cut; the
new mutation test has been type checked but not executed. This closes two
retained-evidence consistency gaps, not the consumer runtime claim: issuance,
claim validation and fresh selection still refuse consumer-context evidence
until the complete applied runtime and lifecycle joins are authenticated.

The signed fixture now selects the exact retained Codex executable as its
direct scoped target. The daemon-side listener transfer, READY wait, prepared
home/CWD and immutable configuration mounts are implemented; retained START
and terminal observations carry their exact identities. The verifier derives
the signed effective environment and CWD independently and compares the
applied launch, then checks the app-server conversation, scripted provider
contacts and frozen candidate. It still deliberately refuses to issue a
qualification claim. Focused component tests and a real signed product
capture pass, but the accepted direct qualification root refused before
process launch: the local fixture node lacks qualified Lillux process-scope
authority. Thus no joined native direct-target positive, malicious negative,
or daemon-restart run has passed. Observation parsing and synthetic receipts
are not external-runtime qualification. Do not enable a claim until a joined
test proves exact pre-release ACK, target-namespace realization visibility,
app-server conversation, provider contact accounting, whole-scope/writer
settlement, frozen candidate, and the negative cuts for wrong/lost ACK,
verifier death, cancellation, restart, and replay.

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

## Historical direct-target implementation sequence

The following records the staged launch-profile work that led to the current
source boundary above. Earlier refusals and intermediate recipe versions are
historical checkpoints, not claims that those intermediate limitations still
apply to the latest source. Their successful component checks do not establish
installed qualification.


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

The generic mount-plan cut is implemented: Lillux can open an existing
canonical directory descendant from a retained descriptor without creating or
chmodding it. RyeOS has a distinct producer-prepared authority that compares
its source descriptor with that exact workspace descendant. For enforced
projectless scratch, isolation now rejects duplicate IDs and overlapping
destinations, mounts the pinned descriptor at the ID-derived destination, and
commits the target CWD and environment in its existing sealed launch plan.
Prepared CWD explicitly refuses pathname-based `{cwd}` policy placeholders;
the adapter starts from the pinned scratch root, while only the target changes
to the prepared destination. The daemon now opens each signed ID only below
the retained private workspace's fixed `prepared/<id>` coordinate, refuses
an absent or non-private directory before reservation, and passes the pinned
set to isolation. Focused plan-inspection and request-construction tests pass.
The shared prepared-directory bound allows sixteen environment bindings plus
one distinct CWD ID; request compilation checks that bound before reservation.
This is not a producer-launch acceptance test: no adapter execution has yet
proved the applied source, target chdir, or writer settlement. The Codex
preparer must also prove exact configuration content and writer exclusion.
Directory identity alone does not establish those claims.

The Codex verifier has a separate parent-owned direct-target preparer. It
stages the signed native request, exact command-tool tree and empty candidate
under `prepared/codex-occurrence`, and the signed baseline plus rendered
command environment under `prepared/codex-home`. The rendered guest CWD uses
the fixed target namespace coordinate, not the old verifier-owned inherited
descriptor path. Its executable points at the selected read-only controller
realization member. The preparer rechecks signed input bytes before START and
can inspect the candidate and guest observation after an independently proven
whole-scope/writer-exclusion fence. Focused source-selection, staging,
tamper-refusal and frozen-inspection tests pass. The direct verifier branch now
calls this preparer, and the signed scenario selects direct Codex. In
particular, a verifier-visible controller pathname does not
prove that the target saw the same read-only mount; that requires applied
namespace evidence. Nor can post-scope byte equality prove what mutable home
content Codex read during execution. The joined test must correlate Codex's
effective behavior and the applied launch with those sealed inputs.

The fixed preparation coordinates are intentionally create-once. After an
uncertain START or verifier restart, an existing directory must not be deleted
and blindly recreated. Recovery must resolve the exact retained attempt and
reopen its preparation under the same authority, or refuse. Until that
reconciliation is implemented, this path remains a preflight component rather
than an admitted qualification scenario.

The fixture's signed producer recipe selects the exact retained
`subject/bin/codex` realization member, finite app-server argv, interactive
input budgets, the fixed prepared occurrence/home IDs, and one signed
loopback ingress. The verifier stages before START, rechecks before release,
and joins transcript/provider/candidate observations only after the daemon's
natural-empty scoped observation. It still refuses to issue qualification
claims. Focused verifier-library, verifier-binary, and signed fixture-authoring
tests pass. Those are not a joined native direct-target run. The verifier
binary now rejects the old `--scenario-driver` mode and requires the signed
direct loopback ingress before callback admission; its parent-owned driver
function and stop test were removed. The old transcript fallback inside the
direct verifier and staging helpers still remain for a later source clean
cut. This source change passed release-mode all-target compilation, not a
native qualification run. Native execution must prove that the rendered
controller executable is reachable as the same read-only realization member
inside the target namespace and that the daemon's settlement excludes every
candidate writer. Exact-attempt restart reconciliation remains open.
