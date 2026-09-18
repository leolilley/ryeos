# Integrated implementation evidence

Date: 2026-09-18 (Pacific/Auckland). Source base: `e89fa09377986dced634ea83b3792ecbe4959445` with the shared uncommitted implementation working tree described by plans 01–06.

This is development evidence, not an installed-host, release, publisher or external-provider attestation. Counts below are recorded only for commands actually run on this working tree. Tests explicitly gated on privileged/disposable infrastructure remain open and are not converted into passing skips.

## Toolchain used

- Rust/Cargo: `1.95.0`.
- Browser checks: Node `20.19.6`, npm `12.0.2`. npm reports this pairing unsupported. The repository-prescribed Node `24.21.0` / npm `11.19.0` rerun therefore remains open even where the current-host command passes.

## Passing integrated gates

| Area | Command / gate | Result |
|---|---|---|
| App/accounting | `cargo test --offline -j2 -p ryeos-app --lib` outside the filesystem sandbox for isolated Unix sockets | 1,033 passed, 1 ignored, 0 failed. Includes F08 v6→v7 migration, bounded high-cardinality admission and paged settle/reopen. |
| State/retained closure | `cargo test --offline -j2 -p ryeos-state --lib` | 792 passed, 0 failed, 0 ignored. Current fixtures explicitly include workspace-output-capture nulls, real attestation signatures and typed v5 qualification evidence. |
| Shared UI model | `cargo test --offline -j2 -p ryeos-client-base --lib` | 442 passed, 0 failed. |
| Field contracts | `cargo test --offline -j2 -p ryeos-client-base --test field_contract_fixtures` | 8 passed, 0 failed. |
| F05 retained binding | `cargo test --offline -p ryeos-api --test external_content_retained_binding -- --nocapture` | 6 passed, 0 failed. Includes real managed bind→retained head→preview→admission and installed-binding substitution refusal. |
| Signed view inventory | `cargo test --offline -j2 -p ryeos-client-base --test view_binding_cutover` | 1 passed. All 45 signed views resolve, verify, compose and validate through the live engine pipeline. |
| Lillux OCI/process control | `cargo test --offline -j2 -p lillux --lib process_control::` | 24 passed, 4 explicitly ignored privileged/disposable-host cases, 0 failed. |
| OCI adapter contract | `PYTHONDONTWRITEBYTECODE=1 python3 tests/development/oci_hook_contract.py` | 7 passed. |
| Local inference I01 | `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest bundles/local-inference/tests/tinygrad_qwen/test_inbox_completion.py` | 5 passed. |
| UI project/seat focused | `cargo test --offline -j2 -p ryeos-ui --test ui_seat --test ui_project_authority` | Seat 5 passed; direct project authority 1 passed; populated-handler dispatch is intentionally ignored in the default target and was run explicitly below. |
| Typed live project root | `cargo test --offline -p ryeos-engine pinned_live_project_never_rebinds_to_a_replacement_path -- --nocapture` | 1 passed. Replacement path cannot change descriptor-rooted content. |
| Compiled UI dispatch | `cargo test --offline -p ryeos-ui --test ui_project_authority compiled_browser_dispatch_keeps_equivalent_paths_and_live_projects_isolated -- --ignored --exact --nocapture` | 1 passed. The test populates handler binaries explicitly, mints real sessions and proves opposite-project substitution cannot widen the result. |
| Renderer static/type | `npm run check:renderer` | 0 errors, 0 warnings. |
| Browser unit/transport | `npm test` | 4 suites passed, including durable seat idempotency and exact transport. |
| Packaged WASM | `npm run test:wasm-contract` | Passed with BigInt generation/time, exact large integer, plain-object ABI and populated replay. |
| Real browser root | `npm run test:renderer-root` outside the sandbox | Passed: overlay/navigation/disabled-reason/focus return, ordered effects and exact JSON/surrogate rejection. |
| Web asset closure | `generate_asset_registry.py --refresh-digests` followed by `--check` | Passed. |
| Bundle-set contract | `./scripts/pkg/test-bundle-sets.sh` | `bundle set contract ok`. |
| Formatting / patch hygiene | `cargo fmt --all -- --check`; `git diff --check` | Passed at the final integrated working-tree boundary. |

## Finding dispositions

- **F01–F04:** source complete. Exact process-root, administrator/controller separation, prepared OCI adoption and bounded generation-aware retirement have positive, negative and crash-cut regression coverage. Intent-only recovery preserves a same-named replacement.
- **F05:** source identity path complete. Real managed binding production feeds durable head, preview and admission. Project-composed authority cannot borrow an installed binding.
- **F06–F07:** retained handle/stable identity direct and compiled integration pass. The final implementation uses typed `AuthoritativeProjectContent` backed by `PinnedDirectory`; it does not reopen a canonical path or globally interpret `/proc/self/fd` strings.
- **F08:** source complete. Schema v7, bounded exact chronology, partial-advisory migration and bounded neighbor admission pass in the full app suite.
- **I01:** source/protocol complete. Installed worker/model execution is not claimed.
- **F09–F12:** source and generated web closure complete on the current host. Pinned-toolchain and installed-entry qualification remain open.
- **Shared-client six:** corrected at their real boundaries; the 442-test library is green.
- **Expanded view inventory:** 45/45 pass the real signed resolution/composition path and refreshed behavior goldens.

## Explicit unqualified boundaries

- No privileged disposable OCI host journey was run. Root credential transition, writable delegated cgroup, hook→bootstrap→workload→poststop/recovery and authenticated attestor evidence remain open.
- No installed signed archive acquisition or real worker process was used for F05.
- No provider credentials, remote node, release promotion, live activation, host provisioning or external spend were used.
- No admitted GPU/model/runtime qualification was run; numerical or protocol tests do not substitute for signed runtime/driver/resource evidence.
- Browser checks have not yet been repeated under the pinned Node/npm versions or through an installed signed-entry/cache boot.

## Rollout note

The accounting ledger now writes schema v7. Rollback to a binary that cannot read v7 is unsafe; stop new admission and use a compatible binary rather than deleting or rewriting ledger evidence. OCI generation journals, external-content heads, project authority and seat receipts likewise remain retained authority/evidence and must not be manually cleared to force startup.
