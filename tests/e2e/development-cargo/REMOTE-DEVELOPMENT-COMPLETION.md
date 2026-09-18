# RyeOS remote development completion plan and live ledger

Updated: 2026-09-18, Pacific/Auckland. Owner: this execution thread.
Status: BLOCKED AT GENERIC RYEOS BOUNDARY — merged-worktree remediation is installed on the primary,
source, and target nodes at v0.5.93 revision `545c716e2a25`. The retained pair
was upgraded in place, then its incompatible epoch-35 execution history and
target project HEAD were explicitly retired with operator approval. Both nodes
are healthy on epoch 39 with current projections; their identities, vault,
remote descriptor, grant, and project binding remain exact. The first fresh G1
product launch fails while constructing its execution realization because a
structured selected-resource property contradicts the realization's scalar-only
property contract. No E2E pass claimed; do not relaunch before the generic fix.

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

Current gate: G0 lifecycle and route checks pass after the clean epoch cut. Configured-operator
push re-established exact target snapshot `21867eea…` with unchanged content tree
`7e09b35c…`; composition reported an absent witness, without establishing which
of the two supplied witnesses was absent. Verify each independently before rebuilding.
G1 is blocked at root `T-99ffc235-c781-eda2-7be3-b22aded526cf` by
`execution realization properties must be scalar or null`. Fix and install the
generic execution-realization property representation, then inspect that terminal
root and issue one new prepared-input launch; never reuse its failed launch ID.
Historical launch IDs, chain heads, replay
indexes, and the former target project HEAD were deliberately retired and must not
be reconstructed or reused. Do not create another node, use a custom runner, copy
a binary directly, or issue bare host-upgrade phases outside the audited installer.

Current implementation checkpoint: `545c716e2 Implement merged worktree
remediation`. It contains the reviewed remote-development and CLI reflow corrections
and is the exact revision reported by all three healthy daemons. Repository HEAD
has since advanced through unrelated UI projection work at `ce1569e80`; that work
is not part of this installed qualification generation. Native verifier live
success is NOT established.

Host-context checkpoint at 2026-09-18T05:38Z: retained source 7423 (PID 24227)
and target 7445 (PID 26242) are healthy on v0.5.93 revision `545c716e2a25`.
Source reset retired 16 chain heads, 48 chain-reference artifacts, 3 scheduler
artifacts, and replay epoch 10. Target reset retired 125 chain heads, one project
HEAD, 375 chain-reference artifacts, 12 thread-runtime artifacts, 3 scheduler
artifacts, and replay epoch 10. Both had zero pending transitions. Target project
status is intentionally separate by principal; retained CAS payload bytes are not
product authority. Source is supervised after the final supported reinstall.

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
- `.ai/config/development/ryeos/remote-worker-recovery.yaml`, its matching Graph,
  and `worker_execution:codex/bounded-turn-recovery`: explicit qualification path.
- `.ai/config/development/ryeos/worker-environment.yaml`: allowed child operations.
- `bundles/standard/.ai/knowledge/ryeos/core/execution/worker-hosted-execution.md`:
  supported worker/candidate authority.
- `.ai/knowledge/ryeos/development/release-process.md`: release procedure; compare
  against current scripts/workflows before following potentially stale examples.

## Fixed topology ledger

These are last-known coordinates; G0 must confirm them without mutation.

| Field | Retained value |
|---|---|
| Repo / branch | `/home/leo/projects/ryeos-next`, `next`; installed implementation checkpoint `545c716e2`; current unrelated UI follow-up `ce1569e80` |
| Source app root | `/tmp/ryeos-remote-workflow-e2e-v091.b4i6IGEs/source-node` |
| Source HTTP | `http://127.0.0.1:7423` |
| Source project | `/tmp/ryeos-remote-workflow-e2e-v091.b4i6IGEs/source-project`, detached `bc58d9b709194a8e13f26f641585e2266ba4ba7f`, tree `7a8ea1a83864345f5d7c2d6a8fffcb2a3b8b4e7c` |
| Source node fingerprint | `e1430400ff3d2d919a362b56fc7e0e72dead08977da3ff83e2a4ec0831a9916e` |
| Source operator | `e70c09cb19bfbbc804fc1da882dad65ccca2a2799471c2f6b4fb5402ce0f1756` |
| Remote route | `qualification` — must resolve to target below |
| Target app root | `/tmp/ryeos-remote-e2e-v091.UyjanrF8/target-node` |
| Target HTTP / UDS | `127.0.0.1:7445` / `/tmp/ryeos-remote-e2e-v091.UyjanrF8/target.sock` |
| Target node fingerprint | `2719d3c2cd8bd3e3dac1f2a91381961fb65afa5c743f86439a46e865391704fd` |
| Target local operator | `56feba567a49ae05a1f09c02c91b32ab477b78a204456db458102431f1d7d796` |
| Target vault identity | `0b29be10b26bea6ac4bb2034c71aee41a35fc34f287d5d482f3b922c7be501b8` |
| Target project display path | `/tmp/ryeos-remote-development.GgHPTg/target-project-current-20260915` |
| Frozen target snapshot / target HEAD | configured-operator snapshot `21867eeaf077ab9b0db162431516bbc2cff3090fb9775c9926cf886ed0bec05e`; tree `7e09b35c0110494cc3f8667e671f4f7706440741673575ca8835283b9a011d4b`; 3,010 entries; source Git `bc58d9b709194a8e13f26f641585e2266ba4ba7f`. Generic doctor remains `deployed: false` because it reports the distinct node-owned view; former configured-operator HEAD `19fd0c01…` is retired |
| Primary installed CLI / daemon | `/usr/bin/ryeos` v0.5.93 revision `545c716e2a25`, sha256 `abd6676b5c5e8ab4749d3ce0a03aef2e882bd1f6e8721fce0e31acd33c2b2110`; `/usr/bin/ryeosd` v0.5.93 revision `545c716e2a25`, sha256 `0b0e893375e02a8276998e0b0a02a293e2867d6ac6a8a3eface653e80aa13488`; both exactly match `target/release` |
| Retained source / target daemon generation | both v0.5.93 revision `545c716e2a25`; supervised source PID 16335 on 7423, target PID 26242 on 7445; healthy after explicit epoch-35 execution-history reset |
| Qualification daemon | `/usr/lib/ryeos-qualification/ryeosd` remains v0.5.87 revision `3a43ed50ee3e`, sha256 `263b937620543e2a72ef7e8d4d19225c48a4e40921c8c6b9a1af56e5080aa28b`; it is not the canonical target service image |
| Target daemon last known | retained canonical target healthy on 7445 at v0.5.93 revision `545c716e2a25`; source-to-target signed authorization and every pinned identity coordinate match |
| Credential profile | `personal`; state/generation require status check, never log tokens |

G0 must fill source/target binary revisions and digests, bundle identities, runtime
epoch, grant digest, actual source HEAD, target-issued HEAD, Git base, host service
association, and credential status. Keep Git revision, CAS snapshot, bundle digest,
and node fingerprint separate. Paths containing GgHPTg do not imply node ownership.

## Product evidence ledger

Current-target captures, not a claim of currently valid consumer bindings:

| Product | Witness | State |
|---|---|---|
| Prepared inputs | `f5d78d646fe68e6af36f3f0e36e6d4f569f445f2e9f03b333a61bd7f041a61cf` | historical capture; individual availability unverified after reset |
| Build support | `60b25f1695600bd9e16b980af6e6f6a6d0fff476cd6f1ad223a89312144ce191` | historical capture; current binding must be resolved after the new target HEAD |
| Built utilities | `e5e28c1487cc3abcd4759706075308c98741e970850eca936a5cae3f9f97b6ba` | historical capture; current binding must be resolved after the new target HEAD |
| Authoring runtime | `9304f0eaca0f4a013a8288957cf844857f5bcf8575bf76a86fac99a87ac78957` | historical capture; individual availability unverified after reset |
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

### G0 — reconcile and freeze prerequisites (running)

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

G0a checkpoint (2026-09-17T11:26:02+12:00): stored binds remain source 7423 and
target 7445; neither canonical daemon is running. Host disk has 3.6 GiB available
(96% used). All discovered `/tmp` runit services are down. Current Git HEAD is
`898670a3d`; checkout Cargo.toml/Cargo.lock are `23345c9b…`/`2bb33708…`, while the
retained source project is `23345c9b…`/`b218b761…`. The prescribed Rust source and
crate manifest still match `def27d76…` and `acb8fd21…`. Remaining G0 inventory that
requires healthy daemons (route, credential, grant, runtime epoch and product API
receipts) is intentionally deferred until the G3a source decision is tested.

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

### G3 — prove the orchestration can satisfy the WHOLE test (running)

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

G3a design checkpoint (2026-09-17): the existing durable command observation and
completion fence are sufficient authority; completion termination already admits an
older fence epoch after a recovered worker attaches at a newer boot epoch. Add a
closed, signed bounded-turn mode option that requires post-completion reattachment.
When selected, session-runtime must retain the completed fence and refuse to begin
candidate termination until the same placement/capsule reports a worker boot epoch
strictly newer than the fence epoch. A restart kills the waiting runtime; ordinary
daemon recovery relaunches it, idempotent command keys recover the exact settled
session/turn without contact, and the newer epoch then releases termination. Keep
the normal bounded-turn profile unchanged. Package the option in a separate Codex
worker-execution profile and separate remote-development workflow/config variant so
ordinary remote work never requires a restart. The operator detects the barrier via
the exact command-observation fence, performs normal node restart, then follows the
same workflow coordinate. No provider-specific hook, ambient FIFO, timing kill, new
grant class, or caller-selected filesystem authority is introduced.

G3a implementation checkpoint (2026-09-17T11:36+12:00): implemented the closed
`require_post_completion_recovery` bounded-turn mode field, strict same-placement /
same-capsule validation, and a strictly newer worker-epoch release condition. The
ordinary profile and graph remain unchanged; a signed recovery profile, graph and
workflow Config select the behavior explicitly. Focused evidence: 12 launch-preparer
tests passed, 7 session-runtime tests passed, 17 Codex bundle contract tests passed,
and the API recovery-workflow selection test passed (641 other tests filtered).
`cargo fmt --all -- --check` and `git diff --check` pass. Live restart proof remains
G4/G5 and is not inferred from these source tests.

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
| Sept 17 11:26 NZST | G0a | host process/service/config inventory | stopped obsolete 7444 and Sep-10 descriptor daemons plus four failed `want up` loops; canonical source/target remain stopped; `.tmp/remote-development-evidence-20260917/g0a-host-inventory.md` | retain fixture data; finish daemon-backed inventory after G3a |
| Sept 17 11:36 NZST | G3a | recovery-qualified bounded-turn source correction | 12 preparer + 7 runtime + 17 bundle tests and focused API graph test passed; signed profile/graph/config | commit; reconcile supported target lifecycle, then freeze G0b |
| Sept 17 12:26 NZST | G0 lifecycle | supported source `ryeos start` after repairing two exact artifacts left root-owned by the earlier sudo runner | source reached ready on 7423 with v0.5.88, then the unsupervised child was reaped when the host command session ended; target is healthy on 7445 | provision source with `ryeos node host setup --confirm` as `leo`; CLI-internal sudo needs an interactive password/cache refresh |
| Sept 17 12:30 NZST | G0 lifecycle | attempted host setup first as root, then correctly as `leo` | root invocation refused the invalid root controller; ordinary invocation reached only the expected expired-sudo boundary | user runs `sudo -v`, then repeat ordinary host setup; no custom runner or node reset |
| Sept 17 12:32 NZST | G0 lifecycle/inventory | source host setup completed by user; ordinary start; both daemon and remote checks | source 7423 and target 7445 healthy on v0.5.88 revision `8c0d738d5bf1`; authenticated route, pinned node/site/vault identities and target enforcement/exclusive-session recovery all match | finish credential/grant/products and freeze target HEAD |
| Sept 17 12:34 NZST | G0 authority | sanitized credential read and exact public-grant audit | CLI `remote run --no-project` serializes obsolete `live_authority`; explicit current contract reached target but existing grant lacks `credential-profiles/get`. Grant also predates recovery worker, Cargo check/test, candidate qualification and registry/vendor producer scopes | authorization reviewer requires explicit user approval for the grouped exact-scope merge; merge will invalidate old grant-digest-bound consumer bindings, which must be rebound |
| Sept 17 13:00 NZST | G0 authority/freeze | user-approved same-origin/same-class grant merge and single configured-operator push | grant expanded without dropped scopes; credential status reached target but `personal` is absent. Push thread `svc-1789605680099-4e72ff92` issued target snapshot `24ec8029…` / tree `71d9dea4…` (2815 entries, 78 uploaded, 5435 skipped) | snapshot is frozen; do not push again. Credential creation/login remains a later explicit authority/user-auth boundary |
| Sept 17 13:20 NZST | G1 source correction | retry of producer-Python activation after grant refresh; `affd9c8db` | predecessor daemon rejected a completed activation whose receipt retained the stale consumer grant binding. Implemented structural validation plus safe reacquisition for missing/released/grant-mismatched bindings; corrupt binding/receipt/head still fail closed. 26 focused API tests, fmt and diff checks pass | install exact daemon and retry the same activation; no producer rebuild required |
| Sept 17 13:45 NZST | G1 install checkpoint | stopped redundant second release build; verified `target/release/ryeosd` | staged v0.5.88 revision `affd9c8db1c1`, sha256 `8bb681d8c7557202544b81e0b760efc2367d8bdc939b7235e4aa157ef80e5660`; Git clean. Source stopped normally; target remains running predecessor `8c0d738d5bf1`. Audited installer attempt performed no mutation because `sudo -n` requires a password | user runs `sudo -v`; then `sudo -n scripts/pkg/install-local-direct.sh --app-root /tmp/ryeos-remote-e2e-v057.2nyR5o/target-node --trust-source-publishers --bundle-set full`; verify target image, restart source, retry activation |
| Sept 17 14:30 NZST | G1 live retry / second correction | installed `affd9c8db`; retried exact activation `9deb5e9a…`, job `external-activation:0bb63d…` | consumer-binding contradiction was fixed, exposing a distinct portable-head contradiction. Three attempts safely exhausted; exact job is Failed, no duplicate job/push/grant. Implemented CAS head advancement plus exact exhausted-ledger reconciliation; fresh-store, cancelled, corrupt and mismatched-operation boundaries remain closed. 27 activation tests + state regression pass; authority and acceptance re-reviews approve | install `ac4bea5bb`, whose staged daemon sha256 is `4b58b0799bcee13d1684b97b86be0d01dc4649f9e0651646c04e4373f8220c93`; source is stopped, target remains live on `affd9c8db`. Run audited installer command, restart source, repeat exact activation to fold the same job |
| Sept 17 15:00 NZST | G1 install / activation recovery | supported full installer after rebuilding the clean `next` CLI | first install safely stopped at a stale cross-worktree CLI expecting `persistent_session.cleanup_authority`; rebuilt `ryeos` sha256 `2ff0f831e8ae693eed45bf8585a3f662f8ac6921aaec1c5e930ba157d2b66232`. Installed daemon sha256 `4b58b0799bcee13d1684b97b86be0d01dc4649f9e0651646c04e4373f8220c93`, v0.5.88 revision `ac4bea5bb65a`. Exact activation `9deb5e9a…` completed on the same exhausted job with receipt `84c92061…`, phase `completed_from_current_bindings`, attempt count still 3 | activation correction is live-proved; no new acquisition, producer rebuild, push or identity replacement occurred |
| Sept 17 15:06 NZST | G1 verifier composition / launch | composed exact current authoring-runtime and prepared-input bindings, then accepted launch `L-20260917000000000000000000000053` | composition thread `svc-1789613869182-f5ac2ba4`; bindings `01db730e…` and `d4ff9e…`. Verifier thread `T-5961667f-a21c-6438-b129-17dd84fdacf4` failed before PID/start with retained `engine_error`; admitted capsule `735e6bc5…` proves exact manifests `462135d8…`, `55027628…`, producer Python `800d4969…`, and correct projectless authority | do not reuse launch 53. Retained error is intentionally redacted and receipts add no process step; run one synchronous verifier request through the same origin-bound remote route so the spawn boundary is returned directly, then correct the generic isolation/spawn defect if confirmed |
| Sept 18 | G0 implementation reconciliation | host lifecycle status plus installed/release hash comparison at source checkpoint `545c716e2` | primary healthy on 7400 at v0.5.93 revision `545c716e2a25`, PID 19430; installed CLI/daemon exactly match release hashes `abd6676b…`/`0b0e8933…`. Retained source 7423 PID 11380 and target 7445 PID 9401 remain healthy at v0.5.91 revision `bc58d9b70919` | preserve both retained nodes and all authority; perform supported in-place upgrade to the exact current committed generation, then revalidate identity/grants/snapshots/products before resuming the native verifier boundary |
| Sept 18 05:38 NZST | G0 lifecycle / clean epoch cut | supported in-place installers, `node reset execution-history`, node status, health, and project-aware remote doctor | both retained nodes now healthy at v0.5.93 revision `545c716e2a25`; source PID 24227, target PID 26242. Operator-approved reset retired source 16 heads/48 refs and target 125 heads/375 refs/12 runtime artifacts/one project HEAD; both retired 3 scheduler artifacts and replay epoch 10 with zero pending transitions. Original node/operator/vault coordinates remain exact; signed authorization and project binding pass | target is intentionally undeployed with no live snapshot HEAD. Push the unchanged retained source project once, record its newly authoritative target snapshot, then resolve current products and resume G1; never reuse retired launch/effect identities |
| Sept 18 05:42 NZST | G0 target freeze | one `remote push` as configured operator from clean detached source Git `bc58d9b…` / tree `7a8ea1a8…` | snapshot `21867eeaf077ab9b0db162431516bbc2cff3090fb9775c9926cf886ed0bec05e`, project tree `7e09b35c0110494cc3f8667e671f4f7706440741673575ca8835283b9a011d4b`, 3,010 entries; 5,900 blobs reused and one uploaded. Content tree exactly matches the former authoritative generation | freeze this configured-operator HEAD; generic doctor correctly reports the separate node-owned view as undeployed. Resolve current products and do not push again unless the frozen source generation changes intentionally |
| Sept 18 05:55 NZST | G0/G1 product continuity | exact `compose-product` for native verifier under snapshot `21867eea…` | first retained-current-HEAD wrapper launch `L-bc114deb…` failed before thread birth (`T-1ccd15aa…`, `launch_admission_failed`) because a unary service was incorrectly wrapped as an accepted root; no product handler contact. Correct projectless configured-operator composition then reached target and returned `retained product witness is absent` | not a substrate defect and do not reuse the failed launch. Historical product payload bytes confer no witness authority; reproduce the authored product chain under the frozen current generation |
| Sept 18 05:58 NZST | G1 prepared-input reproduction / generic blocker | configured-operator launch `L-1c370bec6676a8ebce347df12da51c21`; source remote thread `svc-1789711065965-d99c57b0`; target root `T-99ffc235-c781-eda2-7be3-b22aded526cf` | launch bound to exact snapshot `21867eea…`, project authority `f27736e…`, then failed before start with `execution realization properties must be scalar or null`; PID/PGID null, no successor, no capsule, zero receipts/effects, no producer Tool/provider contact. Source digest-only error `6028cea0…`. Code inspection finds `execution_properties()` stores `selected_resources` as a JSON array while `AdmittedExecutionRealization::validate()` rejects every array/object property | stop without retry. Correct the generic representation/validation contract and cover empty plus nonempty resource selections; install on both retained nodes, then use a new launch coordinate and continue G1 |

Evidence correction (subsequent code review): the 05:55 entry overstates two
conclusions. The unary launch's detailed admission cause was not recovered;
its use of accepted authority alone does not establish the cause. The batch
composition refusal proves at least one supplied witness was unavailable, not
that both were absent or that the reset caused their absence. Inspect individual
witnesses before deciding which producers require reproduction. G0 credential,
product, and evaluator-base checks remain outstanding despite healthy lifecycle
and route checks. Times marked 05:xx in the Sept 18 entries above are UTC, not NZST.

Implementation correction: resource selections now validate through their owning
contract and encode as canonical JSON text, matching target-requirement encoding.
The generic scalar-only realization envelope remains unchanged. The focused
executor realization suite passed all three tests, including production property
construction through state validation, round-trip decoding, identity changes,
and rejection of raw arrays/objects. Formatting and diff checks passed. This is
not a live recovery or remote-launch pass. Installed qualification remains
blocked until rebuilt binaries pass the fresh remote launch boundary. The test
build left roughly 1 GiB free on the host filesystem; release rebuilding and
installation were not attempted. Existing qualification nodes are untouched.

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
