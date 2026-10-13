<!-- ryeos:signed:2026-10-06T01:00:58Z:64131c7aad1f1d8a53e4500bb9dbd091e65622f10ac4c086da16572217e15b1b:yBTEXYDhkMe7dxz+IXysQArSO0iFWdI2AsY+Sf4sNVKThWXOBWifSQuZrRyeEqCql+z2zTYt4zb0AoLy1wJ6Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: README
title: RyeOS Platform Direction and Future Design Map
description: Complete ownership, maturity and trigger map for the application-platform direction and its future mechanisms and consumers
entry_type: reference
version: "1.7.0"
```

# RyeOS platform direction and future design map

Start with [Platform direction and delivery](platform-direction-and-delivery.md).
RyeOS is being developed as a platform for building, running and evolving
applications that combine code, models, tools and services. Authors should be
able to use supported languages, compose capabilities, select environments,
add views, operate applications and publish new generations without rebuilding
unrelated native components.

The first platform milestone is one complete, usable interpreted application
development path. A second materially different consumer establishes reuse.
Independent publication and broader execution extend that experience through
their own qualified contracts. Existing clients and authoring paths need not
wait for every later mechanism. The direction document owns these outcomes;
individual notes own their mechanisms and branch-specific dependencies.

## Reading and status rules

This is a complete map of the future notes, reconciled on 2026-10-06. Maturity
describes the note and its remaining work, not a supported-product matrix:

- **Foundation + deferred**: the owner records an existing basis and bounded
  follow-ups. Recheck the actual selected implementation and qualification.
- **Active source / selected branch**: implementation or consumer work has been
  selected; source review is distinct from build, installed and operational proof.
- **Next candidate**: directly serves the platform milestone; scope and allocation
  still depend on an actual consumer and its missing contract.
- **Deferred / design / discussion / research**: a proposal or claim-specific
  gate, pulled forward only by its stated requirement and evidence.

Labels summarize documented state, including dated observations. They do not
override current implementation checkpoints or declare all old baseline claims
freshly verified. A direction decision does not make all deferred work active.
Rows state the trigger and useful completion; detailed acceptance remains with
the linked owner. The same note may contain several delivery stages.

The index groups documents by responsibility. It does not prescribe a serial
queue: application demand selects the necessary shared work. Specialized
inference, warm workers, learning, source hosting and federation have independent
gates and are not blanket prerequisites for ordinary application development.

## Direction and continuity

| Owner / contribution | Maturity | Trigger and useful completion |
| --- | --- | --- |
| [Platform direction and delivery](platform-direction-and-delivery.md) | Direction | Owns the overall application lifecycle, responsibility boundaries and observable milestones; assess each next slice against an actual author/operator requirement. |
| [Enduring working environment](enduring-working-environment.md) | Discussion | Continuity claim or UX decision → map principal, project, work and authority through a concrete journey; retain unresolved questions. |
| [Substrate growth roadmap](substrate-growth-roadmap.md) | Branch map | Selected execution requirement → follow that branch’s dependencies; inference/self-hosting order does not gate every application. |

## Application authoring and composition

| Owner / contribution | Maturity | Trigger and useful completion |
| --- | --- | --- |
| [Language-neutral authoring and clients](language-neutral-application-development-and-execution.md) | Next candidate | Actual author/client consumer → one usable admitted path, accurate capabilities, diagnostics and two interpreted revisions using unchanged native binaries. |
| [Integration adapters](language-neutral-integration-adapters.md) | Next candidate | Product behavior exceeds existing profiles → admitted procedural implementation and runtime closure; retain shared ownership and demonstrate reuse. |
| [Native modularity and bundle composition](native-modularity-and-bundle-composition.md) | Active slice; broader design | Observed dependency cost → bounded extraction for a real consumer with measured scope; deeper app/state optionality needs explicit inventories and recovery. |
| [Effective non-executable items](effective-non-executable-items.md) | Design | Consumer needs composed non-executable data → one authoritative effective-resolution contract across consumers; verify current source before allocation. |
| [Source versus composed contracts](source-vs-composed-contracts.md) | Foundation + deferred | Concrete source-validation gap → strengthen the owning validation stage while preserving effective-instance contracts. |
| [Shared bundle registration validator](shared-bundle-registration-validator.md) | Foundation + deferred | Third consumer or demonstrated validator drift → shared bounded contract with consistent loader/planner behavior. |
| [Resolution pipeline extensions](resolution-pipeline-advanced.md) | Foundation + deferred | New resolver requirement → add one governed stage with explicit inputs/failures; preserve the existing closed registry. |
| [Project AI surface registry](project-ai-surface-registry.md) | Foundation + deferred | Another deployable surface makes current registry insufficient → discover/admit/sync it without unrelated engine branches. |
| [Shared offline executor](shared-offline-executor.md) | Foundation + deferred | Multiple callers need identical offline launch → shared execution of effective items with preserved protocol and authority. |
| [Command registration extensions](command-registration-advanced-path.md) | Foundation + deferred | Real grant, role or reload requirement → node-owned registration transition; descriptors never grant themselves authority. |
| [Optional operation catalog](node-operations.md) | Deferred | Non-CLI discovery/audit cannot use item refs and commands → bounded operation vocabulary; no duplicate routing or authority. |
| [YAML parser migration](yaml-parser-migration.md) | Deferred | Concrete maintenance/security/compatibility need → selected parser preserves required document and artifact behavior; deprecation alone is insufficient. |

## Execution and resource management

| Owner / contribution | Maturity | Trigger and useful completion |
| --- | --- | --- |
| [Nested execution ownership and evidence](nested-execution-ownership-and-evidence.md) | Foundation + deferred | Selected workload exceeds current parent/child contract → own launch, cancellation, terminal evidence and unresolved resources through failure. |
| [Tool runtime authority](tool-runtime-authority.md) | Foundation + deferred | New callback/resource requirement → exact daemon-derived grants under the applicable callback contract; no launch-capability self-grant. |
| [Runtime delegation extensions](tool-runtime-authority-advanced-path.md) | Deferred | Concrete cross-bundle/delegated operation → intersect declarations, policy and caller limits; preserve distinct thread-local rules. |
| [Provider lifecycle profiles](data-driven-provider-lifecycle.md) | Deferred | Repeated real lifecycle operations → bounded shared profile interpreter; procedural interpreted adapters have their own independent path. |
| [Runtime verification composition](data-driven-runtime-verification.md) | Foundation + deferred | Second real scenario establishes common semantics → reusable signed policy and selected checkers; native execution facts retain their owners. |
| [Leased managed runtime workers](content-addressed-managed-runtime-workers.md) | Deferred | Measured residual startup cost warrants reuse → admitted invocation leases, reset, cancellation and isolation; fixed workers do not prove this class. |
| [Local inference execution branch](local-execution-roadmap.md) | Selected branch; qualification open | Offline-model requirement → selected recorded model/site, bank/replay and bounded evidence; training and sealed claims have later gates. |
| [Sealed local inference](sealed-local-inference.md) | Stronger claim; qualification required | Need re-derivable inference and closed observed scope → two clean-process byte proof under exact artifact/device/numerics identity. |
| [Generation-state capsules](generation-state-capsules.md) | Deferred | Measured prefix/recovery/search cost → recorded restore first; equivalence requires separate qualification; policy-change/incident restoration has additional gates. |
| [Execution identity](execution-identity.md) | Foundation + deferred | New realization/device/claim scope → exact admitted versus observed identity; no device equivalence inferred from content hashes. |
| [Determinism classes](determinism-classes.md) | Foundation + design | Effect consumer needs replay/class semantics → declare complete effects and report divergence; runtime identity comes from realizations. |
| [Durable node effect records](durable-node-effect-records.md) | Foundation + deferred | Cross-run reuse of eligible node work → exact-key zero-execution replay; extend only from measured consumer needs. |
| [Provider-call effect records](provider-call-effect-records.md) | Foundation + deferred | Selected replay/certification/export consumer → retain exact provider-turn evidence and distinguish first contact from replay. |
| [Provider accounting and hard budgets](directive-provider-accounting-and-hard-budgets.md) | Foundation + deferred | New billing/quota/reconciliation requirement → exact reservation ownership; hard spend only with an enforceable cap or proved upper bound. |
| [Native resume extensions](native-resume-snapshot-pinning.md) | Foundation + deferred | Selected native runtime needs stronger continuation → qualified snapshot/version/cancellation semantics; hosted cold transfer remains separately owned. |
| [Scheduler extensions](scheduler-deferred-advanced-work.md) | Foundation + deferred | Concrete cursor, durability or multi-owner requirement → selected fire identity/recovery guarantee; distributed leases are not an authoring prerequisite. |

## Content, environments and publication

| Owner / contribution | Maturity | Trigger and useful completion |
| --- | --- | --- |
| [Environment build system](environment-build-system.md) | Partial implementation + design | A producer or consumer needs exact dependencies → qualify production and simple reuse; native producer stages do not block existing interpreter consumers. |
| [Environment contract builder](env-contract-builder.md) | Foundation + deferred | New environment composition/probe requirement → consistent typed, bounded contributions with protected-name and provenance rules. |
| [Large-content realization follow-ons](large-content-realization-follow-ons.md) | Foundation + deferred | Measured large artifact/composition/storage need → exact bindings, bounded retention and supported consumer use; meaning stays with the application. |
| [Native bundle publication and composition](native-bundle-publication-and-node-composition.md) | Active source; qualification open | Independent bundle updates → exact publication, consumer admission and recovery with unchanged substrate; preserve current activation restrictions. |
| [Native development platform](ryeos-native-development-platform.md) | Deferred end state | Development/release coordination demand → source-to-release evidence and separate publication/activation authority; source hosting follows proven operations. |
| [Reflexive deployment](reflexive-deployment.md) | Deferred | Need RyeOS-owned node upgrade → admitted activation of an already published release; candidates cannot authorize their own installation. |
| [Authenticated envelope metadata](signed-envelope-v2-authenticated-metadata.md) | Deferred | Metadata must carry authenticated meaning → explicit versioned signing/verification transition; current body signatures are not upgraded by interpretation. |
| [Signed JSON configuration](signing-json-config-jsonc.md) | Deferred | A real JSON configuration needs in-band signatures → qualified JSONC/body-preserving format under existing trust owners. |
| [Vault extensions](runtime-vault-advanced-path.md) | Foundation + deferred | Measured access/storage or namespace need → bounded encrypted storage evolution with unchanged authorization and recovery obligations. |

## Interaction and operation

| Owner / contribution | Maturity | Trigger and useful completion |
| --- | --- | --- |
| [Coding worker interaction](coding-worker-interaction-and-capabilities.md) | Deferred consumer | Ordinary worker interaction becomes selected focus → start, observe, answer, inspect, cancel and reconnect with truthful provider capabilities. |
| [Route composition and reload](route-composition-reload-context.md) | Foundation + deferred | Actual route/context reload requirement → staged compatible composition with correct authentication/session lifetime. |
| [Per-row affordance gating](per-row-affordance-gating.md) | Foundation + deferred | A repeated row-level action condition appears → central semantic availability with consistent renderers; invocation still authorizes. |
| [Execution family analytics](execution-family-analytics.md) | Foundation + deferred | Operators need comparisons beyond pairs → bounded change/cost series without invented causality or duplicate accounting. |
| [Chat latency investigation](chat-latency-investigation.md) | Measured foundation + deferred | Observed latency regression or residual overhead → measure cause and optimize selected path before introducing warm execution. |
| [Provider reasoning and latency policy](provider-reasoning-latency-policy.md) | Foundation + deferred | Concrete model/route policy need → admitted settings and truthful effort mapping; domain values remain provider-owned. |
| [Public first contact](public-first-contact-live-node.md) | Deferred presentation | A usable safe live demonstration is selected → genuine public-scoped interaction and sanitized state; cinematic arrival is optional. |

## Distribution and principal boundaries

| Owner / contribution | Maturity | Trigger and useful completion |
| --- | --- | --- |
| [Distributed substrate extensions](distributed-substrate-deferred-advanced.md) | Foundation + deferred | Specific recovery, placement or independent-site demand → qualify that boundary with exact authority/closure; no blanket federation prerequisite. |
| [Hosted-node trust boundaries](hosted-node-trust-boundaries.md) | Foundation + deferred | Unrelated principals/workloads share infrastructure → prove tenant quotas, data/secret boundaries and selected containment before claiming hostile tenancy. |
| [Principal and project registries](ryeos-ui-local-project-registry-and-multitenancy.md) | Deferred | Portable identities or multiple principals require it → exact identity-to-locator mapping and scoped views; tenancy is separately qualified. |
| [Key lifecycle](key-lifecycle.md) | Deferred design gate | Rotation, recovery or cross-era authority is required → explicit succession/revocation and retained-history policy before continuity claims. |
| [Scope profiles](data-driven-scope-profiles.md) | Deferred | Grant-list maintenance becomes a real problem → signed profiles materialize explicit scopes; new capabilities do not silently widen issued grants. |
| [Non-local MCP authentication](mcp-server-auth.md) | Deferred exposure gate | MCP leaves local operator-owned stdio → authenticate audience, replay and exact grants before exposing it. |
| [Portable execution evidence](portable-execution-graph-advanced-path.md) | Foundation + deferred | Another site needs independent verification → bounded disclosed capsule/evidence closure; verification does not grant continuation. |
| [Remote AI sync recovery](remote-ai-sync-advanced-recovery.md) | Foundation + deferred | Required crash/multiprocess guarantees exceed current sync → exact recoverable snapshot application with correct locks and history. |
| [Remote configuration hardening](remote-config-advanced-hardening.md) | Foundation + deferred | Concrete trust or concurrent-edit requirement → bounded operator-owned remote binding and safe updates; discovery is not authority. |

## Application directions and experiments

| Owner / contribution | Maturity | Trigger and useful completion |
| --- | --- | --- |
| [Self-hosted implementation campaigns](self-hosted-implementation-campaigns.md) | Candidate consumer; qualification required | A selected bounded implementation task → owned turn, frozen candidate, disconnect/recovery and explicit disposition; deeper automation follows evidence. |
| [Offline solve campaigns](automated-offline-solve-campaigns.md) | Deferred staged application | Selected unattended offline problem → useful recorded campaign and fresh offline/replay proof; corpus, training and promotion remain distinct. |
| [WANDR research application](wandr-research-agent.md) | Deferred blueprint | Selected research consumer → one evidence-grounded dossier/export using interpreted tools and composition; proposed persistence APIs need admission. |
| [Experience to reusable knowledge](experience-to-reusable-knowledge.md) | Deferred design | Repeated work exposes a useful lesson → scoped adoption, actual selected use and correction with old admissions intact; measure usefulness. |
| [Adaptation and memory evidence](governed-adaptation-and-memory-evidence.md) | Research proposal | A retained-state improvement claim matters → controlled intervention, costs and regressions; attribution alone is not learning. |
| [Creative code spaces](creative-code-spaces/README.md) | Deferred application family | Selected interactive-space consumer → one usable object/action/view composition; its phases do not schedule the platform. |
| [Creative-space dependency roadmap](creative-code-spaces/ryeos-substrate-roadmap.md) | Deferred branch map | A concrete space needs another shared primitive → inventory existing mechanisms and qualify only the missing capability. |
| [Seeded signed spaces](creative-code-spaces/seeded-signed-code-spaces.md) | Deferred application proposal | A curated seed is selected → authored content, permitted actions and view with reusable edits; domain semantics can remain outside Rust. |
| [Renderer to simulation path](creative-code-spaces/threejs-to-simulation-path.md) | Deferred application stages | A space needs richer behavior → qualified next rendering/simulation stage through existing authority; later stages are optional. |
| [World protocol end state](creative-code-spaces/world-protocol-endgame.md) | Deferred application vision | Cross-space composition becomes real → explicit identities, frame permissions and portable content; reuse platform owners. |

## Ownership and maintenance

Keep current operating contracts in their core/bundle knowledge owners. Future
notes may describe an existing foundation to explain the remaining gap; they
must distinguish dated source observations from actual installed qualification.
Completed plans and transient branch receipts belong outside this directory.

For a new proposal, name its application requirement or shared guarantee,
existing owner, concrete trigger and observable completion. Identify whether
the change belongs in signed data, admitted implementation code or native
mechanisms. Link the owner from this map and check for an existing proposal
before adding another roadmap or kind. Update maturity when the evidence changes.

The original integration-adapter and native-modularity notes now have durable
locations here; the preserved worktree is historical provenance only. Earlier
command/verb migration, fixed workers, explicit hosted placement and other
recorded foundations should be understood through current operating knowledge,
not treated as new implementation allocations from this future index.

Review changes for architectural consistency and the complete author/operator
experience. Apply review findings to their owning documents. A successful slice
narrows its future note and updates its operating contract and this map; a review
report does not become a competing specification.
