# RyeOS remote development completion plan and live ledger

Updated: 2026-09-17, Pacific/Auckland. Owner: this execution thread.
Status: REVIEWED — ready for gated execution. G0a is next; no E2E pass claimed.

This tracked-path document is the sole canonical continuation plan. Keep its
checkpoint and append-only journal current and commit material checkpoint updates
with relevant work. `.tmp` pointers are not alternate plans. Protect referenced
local evidence from cleanup; preserve sanitized acceptance receipts in the final
versioned qualification report. Local coordinates below are historical test fixture
data, never product defaults or launch authority.

## Resume here — mandatory checkpoint

Read this document first after interruption/compaction. Read the current gate and
its referenced contracts before acting. Historical transcripts and launch fixtures
are evidence, not current authority. Do not restart the investigation from scratch.

Current gate: G0a read-only inventory, then G3a static orchestration feasibility,
then G0b freeze the accepted test generation. G1/G2 must follow that source review.
Next action after plan review: read-only host-context inventory via supported node,
remote, project, product, credential-status, and host-lifecycle interfaces.
No worker launch, node replacement, release or lifecycle mutation during plan review.

Latest committed fix: `7c4854045 Make installed native verifier projectless`.
Preceding fix: `e829e913f Package native verifier product relationships`.
Both are installed in current target bundles by a successful stopped-node init.
Native verifier live success is NOT established. Last launch 52 failed before that fix.

Last host-context audit: target port 7445 unavailable; v059 log says sudo password
required. Older GgHPTg target has a live descriptor-launched process; leave it alone.
Source liveness is unverified. No host diagnosis should rely on sandbox visibility.

## Scope and finish line

Finish the local two-node RyeOS development proof, integrate its reviewed returned
amendment, close out necessary source fixes, and publish an immutable downstream
release. The user has already authorized this workflow and local operations.
Interactive credential/admin steps still require the user's actual authentication
when technically unavailable; do not treat ordinary continuation as new approval.

The full proof is: exact repository snapshot → authenticated target transfer →
qualified target products → bounded hosted Codex edit → three admitted child
operations → exact completion evidence → post-turn restart/recovery → immutable
candidate freeze/evaluation/qualification → target-signed return → source-head
fence → explicit integration → reviewed Git change → verified release publication.

No completion claim from model prose, build success, daemon health, product capture,
bare-session recovery, or format-only execution. Local evidence does not establish
Railway/OCI hosted-worker capability. Their deployment qualification and Farm/ARC
onboarding remain separate roadmap phases, explicitly reported at handoff.

## Authoritative references (read applicable ones before each gate)

- `.tmp/remote-development-chat-audit-20260917.md`: historical audit and log paths.
- `.tmp/remote-factory-hardening-ui-and-substrate-roadmap-20260913.md`: phase-0 criteria.
- `.tmp/codex-remote-ryeos-development-conservative-closeout-20260913.md`: historical
  recovery evidence; 509 failed after a real turn, 510 proved only bare recovery.
- `tests/e2e/development-cargo/candidate-task.md` and
  `tests/e2e/development-cargo/strict-json-key-amendment.txt`: exact task.
- `.ai/config/development/ryeos/candidate-evaluation.yaml`: base-owned assertions.
- `.ai/config/development/ryeos/remote-worker.yaml` and
  `.ai/graphs/ryeos/development/remote-worker.yaml`: public workflow and driver.
- `bundles/codex/.ai/worker-executions/codex/bounded-turn.yaml`: worker lifecycle.
- `.ai/config/development/ryeos/worker-environment.yaml`: allowed child operations.
- `bundles/standard/.ai/knowledge/ryeos/core/execution/worker-hosted-execution.md`:
  supported worker/candidate authority.
- `.ai/knowledge/ryeos/development/release-process.md`: release procedure; compare
  against current scripts/workflows before following potentially stale examples.

## Fixed topology ledger

These are last-known coordinates; G0 must confirm them without mutation.

| Field | Retained value |
|---|---|
| Repo / branch | `/home/leo/projects/ryeos-next`, `next` |
| Source app root | `/tmp/ryeos-remote-workflow-e2e.yf2N1aE2/direct-source-203962472` |
| Source HTTP | `http://127.0.0.1:7423` |
| Source project | `/tmp/ryeos-remote-workflow-e2e.yf2N1aE2/source-project` |
| Source node fingerprint | `f5c288477d0f254d58ef4e5ea764138f546fbb7bfe79609ee15a06286319fe10` |
| Source operator | `a95068840fd400e29cfe554898fb9395b6db7972a4f6a267bef140b2df51405c` |
| Remote route | `qualification` — must resolve to target below |
| Target app root | `/tmp/ryeos-remote-e2e-v057.2nyR5o/target-node` |
| Target HTTP / UDS | `127.0.0.1:7445` / `/tmp/ryeos-remote-e2e-v057.2nyR5o/target.sock` |
| Target node fingerprint | `7579ab7d6dcf7aed46c4f210b0edf8a4ed472a5fa58250d66ee12ef91014ae9e` |
| Target local operator | `3caf68dda07ecdf77fa07d5e33c52e32562688c81f7d17c26dda1083876826fd` |
| Target vault identity | `dc60a9fba7956d4ac2bdaf6056b4da9f3b7f0220e08c07e0957d3340c447fdec` |
| Target project display path | `/tmp/ryeos-remote-development.GgHPTg/target-project-current-20260915` |
| Last pushed snapshot, NOT yet reconciled | `baf02426e1bd15c095ab92ea614b8e5d6b32fa013bfe1994e4847b15ed31a19d` |
| Target daemon last known | `/usr/bin/ryeosd`, v0.5.87; installed bundles newer than daemon |
| Credential profile | `personal`; state/generation require status check, never log tokens |

G0 must fill source/target binary revisions and digests, bundle identities, runtime
epoch, grant digest, actual source HEAD, target-issued HEAD, Git base, host service
association, and credential status. Keep Git revision, CAS snapshot, bundle digest,
and node fingerprint separate. Paths containing GgHPTg do not imply node ownership.

## Product evidence ledger

Current-target captures, not a claim of currently valid consumer bindings:

| Product | Witness | State |
|---|---|---|
| Prepared inputs | `f5d78d646fe68e6af36f3f0e36e6d4f569f445f2e9f03b333a61bd7f041a61cf` | captured |
| Build support | `60b25f1695600bd9e16b980af6e6f6a6d0fff476cd6f1ad223a89312144ce191` | captured |
| Built utilities | `e5e28c1487cc3abcd4759706075308c98741e970850eca936a5cae3f9f97b6ba` | captured |
| Authoring runtime | `9304f0eaca0f4a013a8288957cf844857f5bcf8575bf76a86fac99a87ac78957` | captured; qualification pending |
| Current-target platform | unresolved | query or produce via supported graph |
| Current-target registry/vendor | unresolved | query or produce via supported graphs |
| Evaluator Python binding | unresolved | verify for frozen base/consumer |

Literal producer Python manifest: `800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf`.
Previous installed-verifier literal binding: `3f3cc336069fd739d4828676ebe630a235f07facfc34ceffa70b1bac6567a527`.
Runtime edit may change effective consumer authority; verify/rebind before reuse.
Record owner, producer root, snapshot, manifest, qualification, consumer digest,
binding, grant generation and supporting API receipt for each selected product.

Do not replay `.tmp/remote-worker-direct-request.yaml`: it still names ffc34c… and
old-target witnesses. `.tmp/current-native-authoring-verifier-run.json` is an input
template with already-used launch 52. Query exact launch status before issuing a
new create-only ID. Read current schemas from installed descriptors/source.

## Operating rules preventing another loop

- One owner mutates these two nodes. Reviewers are read-only and never run live tests.
- Host networking/process/lifecycle probes run in host context with tool escalation
  when needed. A sandbox refusal at `/` is not evidence of a product defect.
- Recognize descriptor-launched executables (`/proc/self/fd/N`); inspect supported
  lifecycle and endpoint authority, not just process names. Readiness log ≠ health.
- Establish normal `ryeos start/stop/node status` in host context. Do not create more
  custom runners, edit root service scripts, or open an upgrade transaction merely
  to try a binary. A necessary upgrade needs exact target/pinned-path verification,
  supported transaction sequencing and a recovery plan before stopping anything.
- Preserve current identities and product stores. Before any justified snapshot,
  grant, binary, bundle or topology change, record why and what evidence it invalidates.
- Retain valid producer captures across consumer rebinding; rebuild only when the
  existing authority contracts require it. Never copy authority objects or keys.
- Tool timeout is not process termination. Retain session handles; query retained
  launch/workflow IDs after timeout instead of issuing duplicate work.
- No manual SQLite reads/writes, unconfined fallback, ambient host tools for workers,
  generic import grants, or silent policy expansion to make the test pass.
- Bound build concurrency and check free space before heavy work. No blanket cache
  deletion/rebuild loop. Preserve live artifacts, state, evidence, and unrelated work.
- Do not promise the next action is the last restart/blocker. Report the completed
  gate, evidence and remaining gates.

## Gates and acceptance evidence

Status values: pending / running / passed / failed / blocked / invalidated.
Each passed gate requires a timestamp, source coordinate, node identity, exact
operation/root IDs and evidence path. Historical claims alone cannot mark a pass.

### G0 — reconcile and freeze prerequisites (pending)

G0a is step 1. Step 2 follows G3a. G0b is steps 3–4 and must wait for G3a source
decisions. Static feasibility review precedes lifecycle mutation and source freeze.

1. Inventory fixed nodes, route, host association, actual binaries, stored endpoints,
   startup state, version/epoch compatibility, credentials, grants and disk budgets.
2. Establish supported target lifecycle in host context; verify daemon health AND
   required enforced filesystem/process scope/network posture. Primary installed
   node and older GgHPTg target are outside this execution's mutation scope.
3. Compare actual task/base hashes and evaluator assertions against the selected
   retained snapshot. Release bumps may have changed Cargo.lock; do not trust stale
   fixed hashes. If needed, author/sign/test corrected base assertions on source,
   retain the same task intent, push once and record target-issued HEAD explicitly.
4. Pin exact Git base/source snapshot/target snapshot/bundle identities for this run.
   Concurrent main-checkout work must not silently change the E2E source project.

Read-only source audit already found stale evaluator assertions: current checkout
AND retained source-project Cargo.toml hash is 23345c9b… whereas evaluator expects
8408289d…; retained source Cargo.lock is b218b761… whereas evaluator expects a3fe1868….
The prescribed Rust base still matches def27d76…. Confirm the immutable selected
snapshot, then correct the base-owned manifest assertions if mismatched; do not
change expected candidate Rust bytes just to accept a different edit.

Exit: filled topology ledger, healthy pair, compatible artifacts, known credentials,
exact task/evaluator base, and approved bounded scopes. Only ask for missing actual
authentication or a new material authority choice after safe checks are exhausted.

### G1 — qualify authoring runtime (pending)

Use supported product APIs to verify existing captures. Resolve corrected installed
verifier projectlessly; ensure signed relationship configs and runtime source are
present. Compose its exact runtime/prepared-input products and literal Python for
the correct owner and current definition. Run a fresh retained launch, observe its
terminal result, then qualify runtime_to_authoring_worker through qualify-product.
Record subject manifest, required authoring_runtime_closed claim, verifier root,
terminal evidence and qualification hash. Capture success alone is insufficient.

### G2 — complete all three child-operation prerequisites (pending)

Resolve or produce current-target platform, registry and Cargo vendor products using
existing admitted graphs. Use supported bounded bootstrap roots/activation if inputs
are missing; identify missing grants precisely before changing them. Bind qualified
authoring runtime to worker-environment and platform/vendor to cargo-check/cargo-test,
plus platform to format-check. Provision evaluator's exact Python/runtime closure
under operator authority, outside worker grants. Verify Cargo.lock/vendor agreement,
offline operation, selected executable closure, required file/space/time limits.
Do not silently reduce acceptance to format-only to avoid vendor preparation.

### G3 — prove the orchestration can satisfy the WHOLE test (pending)

G3a: static source/API feasibility, immediately after G0a inventory and BEFORE
G0b snapshot freeze or G1/G2 provisioning. G3b: installed/live preflight after G2.
Execution order: G0a inventory → G3a → G0 lifecycle/G0b → G1 → G2 → G3b → G4–G8.

Review workflow/driver/worker/runtime and candidate APIs before source freeze.
The current driver follows bounded-turn to a terminal candidate coordinate. Determine
whether bounded-turn automatically freezes/terminates before an operator can restart
after a real settled turn. Identify a supported durable observation/pause boundary
and exact restart trigger; timing a shell kill against a fast terminal transition is
not a reliable acceptance strategy. If this path cannot prove the specified recovery,
implement/test the smallest generic correction within existing lifecycle owners;
do not invent provider-specific orchestration or silently substitute bare recovery.
Source inspection confirms `session_runtime.rs` calls
`terminate_completed_dedicated_session` immediately on completed observation. This
is an actual sequencing issue, not a hypothetical concern. Record a reviewed design
for a deterministic qualification boundary before implementing any change.

Similarly verify the driver result projects into supported candidate evaluation,
qualification and source pull_result. The historical candidate-task doc says generic
Graph return projection was missing; inspect current code to decide if still open.
Current remote_worker_workflows.rs already decodes the Graph terminal coordinate
and queries candidate-result, so the historical missing-projection warning is not
proof that this path still needs implementation. Verify its existing bounded seam.
Record concrete API sequence, accepted request schemas, exact required capabilities,
coordinate ownership, and failure/retry behavior before a model turn.
Verify complete workload-client routing, task payload, evaluator source authority,
credential readiness and target requirements without replaying stale JSON.

Exit: executable step sequence for G4–G7; any source changes signed, focused-tested,
reviewed, installed through supported lifecycle, with invalidated gates rechecked.

### G4 — execute bounded development task (pending)

Launch through signed public remote-worker workflow using reconciled target-local
selections. Record public T workflow ID, internal invocation ID separately, target
launch, root/placement IDs, session, turn, command sequence and initial worker epoch.
Run exact fixture: one prescribed Rust test insertion; admitted format-check,
cargo-check and named cargo-test. For each child retain exact command association,
input candidate generation, admitted capsule, terminal result and test count (one
expected test must pass). Check changes belong to the candidate, not source HEAD.
Preserve completion fence/evidence before deliberate restart.

### G5 — post-turn restart/recovery (pending)

At G3's supported boundary, restart the same target through normal lifecycle.
Prove automatic reattach, newer boot epoch, unchanged lineage/project authority,
and byte-identical authoritative historical completion payload/fence. Exclude
explicitly mutable observation wrappers from byte comparison and name the compared
canonical object/hash. Prove settled commands/child calls were not replayed.
Use status/resume after timeout, with original owner-bound coordinates.
Fail the gate if only a bare session or a terminal candidate survived restart.

### G6 — freeze, evaluate, qualify (pending)

Freeze through the exact completion fence using supported owner APIs (or verify
the same explicit lifecycle operation from the driver). Record chain_root_id,
candidate_snapshot_hash and candidate_validation_hash. Require successful
`service:worker-executions/validate-candidate-closure-and-base` for that exact tuple
and retain its receipt. Use the identical tuple for start-candidate-evaluation and
qualify-candidate; also retain evaluator root/terminal/capsule/item/definition and
parameter digests required by the installed schemas. Enumerate the complete
authoritative changed-path set; reject unrelated edits. Run base-owned immutable
candidate evaluator for strict-json-escaped-keys. Match expected Rust bytes and
unchanged manifests/lockfile against G0's admitted assertions. Check candidate-
supplied evaluator/config tampering refusal using an isolated installed direct-Tool
negative fixture on the current target and base-owned evaluator; source-only tests
are supporting evidence, not a substitute. Never corrupt the accepted candidate.
Retain independent evaluator
root/result and qualification testimony separately from worker child evidence.

### G7 — return and integrate (pending)

Obtain target-signed candidate-result evidence and validate exact workflow, owner,
base and candidate. This attestation does NOT assert evaluator qualification; verify
the separate accepted evaluation and qualification refer to exactly that candidate
BEFORE invoking pull-result. Review the complete changed-path set first.

`pull-result` already fetches AND atomically applies to the live source tree; it is
the explicit source integration operation, not a staging-only download. Do not
invoke it speculatively or follow it with a second apply. It does not advance either
principal-scoped project HEAD. Check both recorded HEAD/base authority and live-tree
drift as distinct fences. Return through the source-owned workflow and authenticated
object closure. Prove BOTH principal-scoped HEAD drift and live-tree drift refusal via
focused isolated fixture or equivalent existing test; do not destroy the clean live
source to demonstrate it. Verify live source equals authorized base before pull.
Review resulting exact diff, retain return/apply receipt, and perform focused
post-integration checks. Ordinary Git commit/push remains outside worker authority.

### G8 — close source and publish downstream release (pending)

Review necessary fixes and returned amendment, preserve unrelated work, run affected
tests and signed closure checks, leave relevant Git state clean. Record evidence
coverage for exact source and installed payloads; if qualification used mixed
generations, state what transfers and what requires a focused rerun.

Reconcile real remote branch/tag/release status at execution time, choose unused
patch version (do not assume v0.5.89). Follow release knowledge checked against
current scripts: update all version locations and lockfile; merge next into main
in appropriate clean worktree; tag main immutably; push branches then tag. Never
force-reset a preexisting local branch from an outdated runbook example.
Monitor publication and verify all required release archives/checksums, three image
digests, signing/provenance/SBOM and workflow source revision. Reuse one release Bake
solve; do not duplicate artifact builds or turn publication into a full test runner.
Report immutable downstream coordinates and remaining deployment-specific gaps.
Do not redeploy unrelated downstream services under the release authorization.

For interruption, record workflow run ID, immutable tag/revision and each output's
publication state before retry. Follow the runbook's Interrupted release recovery
section: verify existing signed outputs before reuse; do not overwrite consumed tags
or infer integrity from mutable staging. Partial workload-client archive/checksum
pairs are ambiguous and cannot be reconstructed as authority. Prefer a new patch
where necessary; deleting published assets requires its own concrete authorization.

## Evidence / mutation journal (append; do not erase failures)

| Time | Gate | Operation / source | Result and evidence | Invalidations / next action |
|---|---|---|---|---|
| Sept 16 | historical | e829e913f | installed relationships; live compose succeeded | verifier execution still pending |
| Sept 16 | historical | launch 52 | missing project_path runtime template | terminal failed; new launch needed after status check |
| Sept 16 | historical | 7c4854045 | signed runtime fix; 21 tests; Standard validation; bundle refresh succeeded | consumer definitions/bindings need recheck |
| Sept 17 | G0 audit | host read-only check | 7445 unavailable; v059 sudo authentication error | resume after plan reviews |

Before every stop/compaction/turn handoff: update this top checkpoint, gate states,
in-flight process/tool session IDs, retained launch coordinates, exact next operation,
and any blocker. Never store credentials/private keys/raw authorization headers.
Evidence files go under `.tmp/remote-development-evidence-20260917/` with descriptive
names and exact receipt IDs, redacted before saving. This Markdown indexes evidence;
it is not a second runtime authority store. Update it after each material outcome.

## Review record

First independent review completed with changes requested; all findings incorporated:

| Reviewer | Finding | Resolution |
|---|---|---|
| acceptance + authority | Installed tampering proof weakened to source tests | G6 requires separate installed negative fixture |
| acceptance | Closure/base validation omitted | G6 requires validation receipt and exact candidate tuple across evaluation/qualification |
| authority | Pull-result's live apply boundary ambiguous | G7 gates atomic pull/apply on prior review/qualification; separate HEAD and live-tree fences |
| execution | Source feasibility after expensive provisioning | G3a now precedes G0b and G1/G2 |
| execution | Canonical context only in ignored .tmp | Canonical tracked-path plan plus README pointer; protected evidence and versioned final report |
| execution | Interrupted release procedure omitted | G8 requires exact per-output recovery and immutable-tag rules |

Re-review completed 2026-09-17 against the revised canonical document:

- Acceptance reviewer: approved, no residual blocking findings.
- Authority/lifecycle reviewer: approved, no residual blocking findings.
- Execution/release reviewer: approved with G0a ordering clarification; applied.

Review scope was read-only documentation/source inspection. No reviewers mutated
nodes or executed qualification. G3a's deterministic post-turn recovery design
remains an explicit implementation gate, not proof of existing product capability.
This version includes all six substantive findings and the ordering correction.
