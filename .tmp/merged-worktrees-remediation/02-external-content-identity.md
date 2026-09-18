# External-content consumer identity implementation: F05

Status: declaration-owner correction implemented with managed-producer→retained-head→preview→admission source integration; installed acquisition/process qualification remains open. Parent plan: [README](README.md).

## Implemented result — 2026-09-18

`consumer_authority` now classifies from the admitted effective definition rather than the outer pinned subject. A fixed bundle-owned declaration remains `InstalledBundle` under a pinned launch. A project ancestor/reference or resolved product-selection projection makes the consumer `PinnedProject`; missing exact generation authority refuses. The bundle root's own source closure is not misclassified as a project contributor.

The regression creates a signed temporary bundle, calls the real managed activation binding producer, publishes through the durable import/head path, and then uses production preview and admission. It also proves that an installed binding cannot substitute for project-composed authority and that the exact pinned binding can admit. This closes the source identity disagreement without weakening publisher, manifest, grant or effective-consumer digest checks.

The test does not acquire an installed signed archive or spawn a real worker process. Those are retained as explicit qualification boundaries in plan 06/07.

## Scope

Primary owners: `crates/daemon/ryeos-app/src/external_content_admission.rs`, `operator_external_content.rs`, engine composition/provenance contracts, `crates/engine/ryeos-executor/src/execution/persistent_session.rs`, and managed activation in `crates/daemon/ryeos-api/src/handlers/external_content_activate.rs`.

Fixtures: Codex default environment, environment activation, project-selected product environments/verifiers, command/session launch and recovery tests. Also inspect OpenCode and other users of the generic content-dependency path rather than fixing only a Codex name.

## Required identity distinction

| Effective consumer | Required admission identity |
|---|---|
| Unchanged trusted bundled fixed-pin dependency, independent of outer project | Installed-bundle consumer tied to verified publisher/ref and existing exact binding checks |
| Bundled executable/config whose actual effective closure depends on pinned project composition/products | Pinned-project consumer tied to exact admitted generation/effective closure |
| Project-authored consumer | Pinned-project consumer with existing project admission requirements |
| Missing or ambiguous provenance | Refuse; do not probe alternate binding namespaces |

The outer subject being pinned is not enough to choose the second row. Conversely, bundle-owned executable bytes do not prove the first row if their effective relationships come from a project. `external_product_slots: []` is a useful regression fixture, not a sufficient generic authority classifier by itself.

## Implementation steps

1. Add failing tests for managed activation of the shipped default environment followed by preview and launch under current project HEAD. Assert that activation and admission derive the same consumer/head identity before any process contact.
2. Inventory provenance already retained by `ResolutionOutput`/composition, selected-product ownership and pre-realization digest calculation. Define one deterministic classification based on the admitted effective closure. Do not use mutable filesystem scans, item-name allowlists, endpoint names or discovery of whichever binding happens to exist.
3. If current provenance cannot prove independence, extend the owner-layer admitted representation explicitly. Include all project-dependent composition sources, not just source files or product-slot presence. Preserve signature verification and declaration ownership.
4. Route binding creation, preview, admission and validation through the same classification contract. Change managed activation only if its declared consumer really changes; do not turn every reusable environment into a per-project activation as a workaround.
5. Keep worker admission and environment content admission distinct: the former's existing projectless subject does not license erasing project context from all environment dependencies.
6. Preserve selected-product matching, effective consumer digest, exact manifest and current authorizer-grant checks. Failure remains before execution/release.
7. Validate local current-head and remote pinned launch through shared admission; include the v0.5.93 16-GiB ceiling correction so an unrelated preparer ceiling does not mask the test.

## Retained bindings and recovery

Existing InstalledBundle heads for unchanged consumers should remain usable without rewriting them. Existing PinnedProject bindings and admitted capsules must retain their exact authority. Recovery must consume retained admission, not reclassify historical records using current code and silently select another head.

Investigate whether the affected revision has already created generation-scoped heads for otherwise unchanged consumers. Specify their explicit inspection/rebind procedure if needed. Never retarget or delete them automatically. A stale grant, revoked publisher, wrong manifest or changed effective closure must remain a refusal even when another binding variant exists.

No schema change is assumed. If classification evidence is added to a retained contract, update versions/digests/decoders and historical tests before publication; do not retrofit new bytes into an old digest identity.

## Regression matrix

- Default activated bundled environment + local current-head: preview ready and actual launch admitted.
- Same environment + remote pinned generation: identity agrees; configured-operator and source-node lanes remain distinct.
- Same unchanged bundled environment used by two project generations: reuse only if independence is proved.
- Bundled consumer composing project-selected product: correct pinned binding accepted; installed binding rejected; another generation rejected.
- Project-authored consumer: project signature/generation required.
- Project override/composition added to an otherwise bundled dependency: no incorrect promotion to reusable bundle authority.
- Manifest/publisher/effective closure changes, refreshed/revoked grants and head divergence: exact refusal/rebind behavior.
- Crash/restart after accepted capsule: same retained realization, no fresh mutable resolution or second execution.
- Default environment missing content: honest not-ready, no automatic acquisition outside authorized activation.

## Acceptance

Close F05 only with the actual activation→preview→pinned-session integration, plus the negative project-dependent case. Pure unit tests of the classifier alone are insufficient. Record exact binding subjects and capsules in test evidence without recording credentials. Update affected signed activation/hosted instructions through the established publication process only if the supported workflow changes; the preferred result preserves the documented workflow.
