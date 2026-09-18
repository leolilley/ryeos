# Merged-worktree remediation implementation plan

Date: 2026-09-18. Planning snapshot: `e89fa0937` (v0.5.93).

Status: **source implementation and integrated regression closure are complete on the shared working tree; installed-host, pinned-toolchain and external-provider qualification remain open**.

Source evidence: [merged-worktree review](../merged-worktrees-review-2026-09-17.md), reviewed at `83f32c890`. Since that review, `97ebc7a27` aligned the structured-session content ceiling to 16 GiB and added a validator test; `e89fa0937` bumped release versions. Neither change closes F01–F12 or I01. Preserve that ceiling correction and test it with the environment-admission fixes.

These are unsigned planning documents, not installed RyeOS policy, signed knowledge, release approval, or authority to mutate a host. Names of proposed APIs/tests below are design suggestions, not claims that those APIs already exist. Recheck HEAD, repository instructions and relevant source before implementation.

## Implementation progress — 2026-09-18

- F01–F04 are implemented in Lillux, the OCI hook and node host runtime. Focused process-control, host-runtime and contained contract/product tests pass. The disposable privileged hook→bootstrap→workload→poststop/recovery qualification remains open and no live host was mutated.
- F05 restores declaration-owner provenance for bundle consumers under a pinned outer context. The source integration now exercises the real managed-binding producer, durable retained head, production preview and production admission; installed archive acquisition and a real spawned process remain qualification gates.
- F06–F07 use retained descriptor ownership plus a separate canonical query identity. Direct two-project tests and the populated compiled browser-session dispatch journey pass, including equivalent-path selection and opposite-project substitution refusal.
- F08 is implemented as schema-v7 exact paged settlement. Chronology is `(start_tick_ns, end_tick_ns, attribution_id)`, v6 partial advisory evidence migrates without fabricating a settlement timestamp, all readers are bounded, and high-cardinality admission uses indexed predecessor/successor probes. The full app suite passes.
- I01 is implemented with atomic terminal publication/current-request transition. Deterministic framing, overlap, cancellation, broken-write, repeated-main-loop and authoring tests pass; bundle publication/admitted execution remains separate.
- F09–F12 are implemented at source and generated-asset level: plain-object/null/BigInt WASM semantics, lossless JSON, durable idempotent seat append/recovery, shared overlays/notices, exact semantic choices/folds, group selection, pointer/keyboard focus and instance-addressed cursors. Svelte checks, Node transport/session suites, populated packaged-WASM checks, production renderer build, real headless interactions and asset-registry verification pass on the current host. The prescribed pinned Node/npm environment and installed signed-entry boot remain separate gates.

## Workstream index and complete finding map

| Plan | Findings / additional obligations | Owner boundaries |
|---|---|---|
| [01 OCI lifecycle](01-oci-lifecycle.md) | F01–F04 | Lillux primitives; external host adapter; node bootstrap consumes authority |
| [02 External-content identity](02-external-content-identity.md) | F05 | Engine provenance; app binding identity; executor content admission |
| [03 Project authority](03-project-authority.md) | F06–F07 | Retained filesystem handles; backend projection identity |
| [04 Resource accounting and inference](04-resource-accounting-and-inference.md) | F08; inherited I01 | App ledger/pool; accounting contracts; bundle-owned worker protocol |
| [05 Browser/shared-model integration](05-browser-integration.md) | F09–F12; launcher/sidebar design gap | Rust semantic model; typed browser transport/presentation |
| [06 Tests and qualification](06-validation-and-qualification.md) | All findings; six reviewed shared-client regressions; backend/browser/OCI/GPU/remote evidence boundaries | Unit, integration, publication, installed-host gates remain distinct |
| [07 Implementation evidence](07-implementation-evidence.md) | Exact current-tree results and explicit unqualified boundaries | Source evidence; not an installed/release attestation |

Every finding must retain its ID in regression tests, implementation notes and closure evidence. A refuted finding needs a concrete counterexample/call-chain correction; do not silently drop it. The source review did not establish a containment escape, and inherited I01 must not be misattributed to the recent merge.

## Non-negotiable architecture constraints

- Keep OS process/root/cgroup identity and mechanics in Lillux. Neither daemon nor bundle code should implement a second privileged provisioning path.
- Preserve node/operator/publisher authority distinctions, exact signed generations, attachment-before-execution, and current live-owner release checks.
- Keep native scopes, hard-contained OCI, externally fenced providers and trusted process groups separate. Do not add automatic fallback between them.
- Keep historical admitted recovery immutable. New admission rules do not authorize reinterpretation of old capsules, bindings or financial evidence.
- Keep CAS/events authoritative and SQLite projections rebuildable where that is their existing role. Operational ledgers retain their explicit transactional role.
- Keep UI semantics in the shared Rust model and signed content. Browser code transports and renders; it does not infer execution authority or product readiness.
- Never address a failure by dropping retained evidence, loosening signature/ownership checks, fabricating zero usage, broadly raising limits, or changing denied actions into defaults.

## Sequencing and merge boundaries

1. **Establish reproducible failures and safe build capacity.** Preserve the original review; add targeted regression fixtures before changing behavior. Check disk/toolchain availability. No broad cache deletion or host mutation is implied.
2. **Independent implementation lanes:** OCI F01–F04; content identity F05; project authority F06–F07; accounting F08 and worker I01; browser F09–F12. These can be developed independently, but share a fixed reviewed base and must be rebased/retested before integration.
3. **Within OCI:** process-root access → administrator validation → prepared-root adoption → teardown/recovery. Earlier failures mask later ones. Merge with one real hook-to-bootstrap test rather than claiming success after the first error disappears.
4. **Within project authority:** define lifetime-bearing handle and stable query identity together, migrate every caller, then run source-to-handler integration. Merely keeping an FD open does not fix SQL identity.
5. **Within browser:** stabilize WASM/HTTP contract and shared events before component interaction tests; then regenerate/publish the exact asset closure. Backend project fixes are required for the combined current-project journey.
6. **Within accounting:** implement exact bounded-memory settlement and the matching startup verifier/readers together. Do not turn the current representation cap into worker-lifetime policy. Any independent resident-lifetime policy must distinguish pooled retirement from authorized dedicated continuation. Worker I01 can land separately but needs combined pool-reuse coverage.
7. **Integrate:** run the cross-stream journeys in plan 06, then publish signed development artifacts using existing repository tooling. No hand-copying individual binaries/assets into an installed bundle.
8. **Qualify:** installed OCI, remote development and admitted GPU execution each require their own named environment and evidence. Passing one does not close another.

Recommended commit boundaries: failing test → owner-layer implementation with tests → caller migration → integration tests → generated/signed artifacts and documentation. Where intermediate commits would expose an incompatible interface, keep implementation and callers atomic. Never publish a half-migrated wire or authority contract.

## Retained-state and deployment rules

Each implementation must document whether its wire/schema changes. If unchanged, prove old valid records still decode with identical meaning. If changed, define an explicit versioned decoder/migration and refusal behavior; do not introduce unversioned reinterpretation.

Before rollout, inventory affected retained OCI lifetimes, content bindings/capsules, resource operations/holds and UI seats using read-only tools. Preserve exact coordinates and previous binaries. Rollback is safe only if the old binary understands newly written durable state; otherwise stop new admission and recover with the compatible implementation. Never delete host leases, attribution rows or activation heads to make the old binary start.

Planning here does not authorize credential ceremonies, provider contact, live activation, host provisioning, volume reuse, release promotion or key/signature changes. Those are separately controlled implementation/qualification steps.

## Closure checklist

- [x] F01–F04 source: exact process-root, administrator/controller split, prepared-root adoption and bounded dead-tree recovery pass focused positive/negative/crash tests. Privileged installed lifecycle qualification remains open.
- [x] F05 source: managed binding production, pinned preview/admission and project-dependent substitution refusal are covered. Installed acquisition/process launch remains open.
- [x] F06–F07 source: retained handle, stable query identity and typed descriptor-root resolution pass direct and compiled browser dispatch integration without widening scope.
- [x] F08 source: future and retained over-cap operations settle exactly under migration/retry/reopen, chronological paging is bounded, and accounting does not impose worker termination.
- [x] I01: immediate reuse after final/error is deterministic and safe at the protocol/main-loop layer; installed bundle qualification remains recorded separately.
- [x] F09–F12 source/assets: populated real-WASM, exact transport, durable seat and real browser interaction tests pass.
- [x] Design gap and shared-client failures have explicit dispositions and regression evidence; all 45 signed views pass the composed effective-item path and behavior goldens.
- [x] The full `ryeos-app` library suite passes on the integrated working tree; remaining affected suites are recorded in plan 07.
- [x] Browser generated bindings/assets and their asset registry match the tested working tree. Installed signed-entry/cache qualification remains open.
- [ ] Installed qualification claims are separately backed by fresh exact evidence, or remain explicitly open.

Use the evidence template in plan 06. A checked implementation-level box is not an installed-host or release qualification claim.
