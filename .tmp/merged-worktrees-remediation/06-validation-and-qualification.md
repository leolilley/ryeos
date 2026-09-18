# Regression, integration and installed qualification plan

Status: integrated source evidence collected; installed/privileged qualification remains explicitly separate. Current results: [07 Implementation evidence](07-implementation-evidence.md). Parent plan: [README](README.md).

## Preparation

Record exact source SHA, worktree cleanliness, toolchains, dependency realizations and available disk before builds. The previous backend build exhausted disk; do not start parallel cold builds without a budget. Choose a sufficient existing build location or seek approval for identified disposable cache cleanup. Do not delete broad workspace/user caches.

Use repository build-and-test and publication instructions. Execute proposed tests against real production implementations. Existing source-string assertions, independent Python state models and synthetic UI previews remain useful but are not substitutes for implementation-coupled integration.

## Gate A: targeted regressions

| ID | Required evidence | Explicit negative case |
|---|---|---|
| F01 | Rust exact-process-root primitive traverses intended descendants | Symlink/replaced root/process refuses |
| F02 | Root validates selected account, then actual probe runs non-root | Wrong owner/account not bypassed by root |
| F03 | Actual hook output boots through mounted-root consumer | Native/mount-root mismatch or wrong mount refuses |
| F04 | Empty nested subtree retires and release recovers across cuts | Live/replaced descendant blocks reuse; sibling survives |
| F05 | Managed default activation then real pinned launch succeeds | Project-dependent consumer cannot borrow installed binding |
| F06 | Returned owner survives use under async yield/FD churn | Replacement never switches authorized content |
| F07 | Canonical project rows appear in compiled-session source reads | Other project/principal remains excluded |
| F08 | Multi-page current/retained settlement and startup verification work | Corrupt/overlapping evidence refuses without losing hold or terminating a worker |
| F09 | Actual populated WASM → exact HTTP bytes → daemon deserializer; durable seat append identity | Permanent refusal stops retry; lost ack/restart does not duplicate |
| F10 | Real shared overlays/notices rendered and accessible | Disabled action cannot execute through renderer |
| F11 | Group member selection leaves workspace unchanged | Stale member does not select another group/workspace |
| F12 | Pointer focus/selection and next keyboard action agree | Inert selection does not manufacture an activation |
| I01 | Immediate next request after terminal succeeds deterministically | Genuine concurrent request still rejected |

## Gate B: shared-client failures from the review

All six have explicit corrected tests and dispositions. The full shared-model library passes 442/442. The expanded inventory gate additionally found and corrected two invalid newly landed signed views; it now resolves/composes all 45 through the live signed engine path rather than validating raw `extends` YAML.

1. `ui::content::tests::inputs_plural_list_form_is_rejected`: assert rejection at the actual deserialization boundary; do not unwrap invalid input or reallow plural inputs.
2. `ui::content::tests::project_section_null_collection_yields_no_rows_not_a_dump`: use the current section/source-channel schema, retain the null-collection/no-dump assertion.
3. `ui::reducer::affordances::tests::sections_view_without_a_loaded_source_shows_an_empty_group`: repair source fixture shape and still assert honest empty/unloaded behavior.
4. `ui::field::tests::identical_cross_source_facts_converge_despite_distinct_provenance`: investigate rejection/deduplication with valid field facts; do not assume fixture drift. Preserve genuine convergence and divergent-ID namespacing companion coverage.
5. `ui::reducer::input::tests::feeds_input_drives_its_source_param`: align fixture ref with its compiled binding and assert exact source identity, query injection and unchanged static parameters.
6. `ui::reducer::tests::duplicate_cancel_is_rejected_while_pending`: supply valid compiled command authority; assert one initial dispatch and duplicate refusal. Missing authority must still refuse.

Run the entire affected shared-client suite after targeted tests; document any newly exposed failures separately. Never turn failures into ignored tests just to restore a green count. The current integrated run also exposed stale current-contract fixtures in the broader `ryeos-state` suite; those are tracked separately in plan 07 and must be repaired at the fixture producers, not hidden with permissive decoder defaults.

## Gate C: backend and browser integration

Use temporary, isolated test state; do not contact the current user's nodes or providers. Add fixtures for the following journeys:

1. **Activation→worker:** exact managed default environment activation, pinned project HEAD, signed CLI/API command, preview/admission, accepted capsule, restart/recovery. Repeat with a genuinely project-composed product and wrong binding. Use a deterministic test worker for non-provider execution where appropriate; live credential flows belong to installed qualification.
2. **Project→assistant:** mint compiled session, populate canonical project threads/approval/candidate state, fetch through signed source coordinates, render attention, inspect exact candidate, test unauthorized subject and project replacement. Preserve candidate-read versus acceptance/publication distinction.
3. **Resource→pool:** reserve/attach/issue/release; thousands of deterministic short requests; cleanup/multi-page settlement; database reopen/startup verification; restart/outbox replay; immediate worker reuse. Include retained over-limit fixture and chronological order differing from attribution-ID order. Accounting page boundaries must not force worker rollover. Test any separately admitted pooled/dedicated lifetime policy on its own authority.
4. **Browser→daemon:** real populated WASM and HTTP transport schemas, terminal errors, unknown delivery, seat replay, session turnover, pointer then keyboard interaction, independent view drafts and launcher notices.
5. **OCI→controller:** actual hook binary, protected binding, actual entrypoint/bootstrap and non-root process, followed by cleanup/recovery. A privileged disposable test environment is needed; ordinary CI should explicitly report unavailable rather than skip and label passed.

Recheck previously reviewed remote safeguards: lost launch acknowledgement, concurrent driver exclusion, exact acceptance/settlement facts, selected-product recovery, refreshed/revoked operator grants, completed replay without recontact, and historical authority-family preservation. No fallback to trusted/external lanes to make a test pass.

## Gate D: existing repository checks and artifacts

Commands below are starting points for implementation, not results. Use the repository's supported environment and exact lockfiles. Expand targeted Rust test selectors once new test names are known; do not assume offline dependencies are available.

```sh
cargo test --offline -j2 -p ryeos-client-base --lib
cargo test --offline -j2 -p ryeos-app --lib
PYTHONDONTWRITEBYTECODE=1 python3 tests/development/contained_workflow_packaging.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/development/hosted_oci_runtime_verifier.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/development/oci_hook_contract.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/development/contained_workflow_products.py
./scripts/pkg/test-bundle-sets.sh
./scripts/ci/test-daemon-image-init-policy.sh
PYTHONDONTWRITEBYTECODE=1 python3 crates/daemon/ryeos-ui/generate_asset_registry.py --check
```

Also run affected Lillux, host-adapter, executor, API, UI and accounting tests through their existing manifests. Run the dependency-layer checker and relevant authoring/model/Codex/development tests from the review. Admitted worker protocol tests need the exact `RYEOS_LOCAL_WORKER_TEST_WORKSPACE`, not a guessed ambient Python/model directory.

From `crates/clients/web`, with pinned Node 24.21.0/npm 11.19.0 and the lockfile-retained Playwright browser:

```sh
npm run check:renderer
npm test
npm run test:wasm-contract
npm run test:renderer-root
```

Recheck package scripts at implementation time. Missing dependencies/browser realization are blocked checks, not passing skips. Add the populated regression cases to maintained suites. The empty contract smoke remains insufficient by itself.

Publish generated bindings, WASM, renderer and signed bundle/asset inventory via existing tooling after source tests pass. Do not leave stale signatures, bypass the registry root, install single binary copies, or claim that a preview validates installed boot. Test the resulting complete artifact set and record its exact digests.

## Gate E: installed qualification — separately authorized environments

### OCI

On a named disposable host, collect authenticated evidence for image/source/hook, signed node policy, account/app root, boot, init birth, C/R physical identity and protected binding generation. Exercise actual scoped descendants, detach/nesting, writer exclusion, freeze/kill/cancel, daemon death/restart, container replacement, volume cleanup/reuse and unrelated-process safety. Include all negative delegation/binding cases from plan 01. Update verifier/attestor configuration only through its authorized signed process; a checklist cannot self-promote to an installed claim.

### Remote development

Reconstruct current source/target authority and installed binary/bundle identities before the ledger's remaining gates. Re-run the actual accepted worker→candidate→freeze/evaluate/qualify→return/import/release workflow against the new artifacts. Preserve exact configured-operator and target-key coordinates. Do not reuse old v0.5.91/v0.5.92 testimony to qualify v0.5.93 or later. External-provider cleanup remains a distinct unmet capability unless its independent adapter/receipts are actually provided.

### GPU/inference

Numeric conformance is not runtime acceptance. Resolve the admitted libc/driver execution closure through signed runtime authoring/activation, not host library borrowing. Verify exact backend/model/resource, visibility versus isolation, resource exclusivity, occupancy deadline under daemon death, reuse/cancellation and monetary settlement. Retain numerical evidence tied to that actual admitted execution; do not relabel the earlier glibc Modal process as musl runtime qualification.

Unavailable environments remain open blockers for their claims, even if source fixes and unit suites are complete. Plans do not authorize provisioning, credentials or external spend.

## Evidence and closure record

For each finding, append a record during implementation containing:

```text
Finding / implementation commit:
Reviewed integration SHA and artifact digests:
Reproduction before fix (command, fixture, observed failure):
Positive regression after fix (command, result):
Negative authority / replacement test:
Crash/retry/recovery test:
Retained-schema / migration disposition:
Affected suite results (passed / failed / blocked, never conflated):
Installed qualification evidence or explicit unqualified boundary:
Remaining risk / follow-up:
Reviewer and closure decision:
```

Do not edit the historical review into a retrospective all-clear. Link closure evidence to it. Use separate milestones for source implementation complete, integrated artifacts tested, and installed qualification accepted. A rollback must respect new durable schema readability and retained live-owner liability; when that cannot be proved, stop new admissions and recover with compatible code rather than deleting state.
