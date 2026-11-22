<!-- ryeos:signed:2026-10-06T01:00:58Z:ab91fa51cc378ddd8d71656718c8120c94ed78d1016d19a83c37706deff7c30d:msdVlQNSyMMJ/2eccINGGpVXH6bsEffFNwSTjZEADXQVDznAepx3rUcT/1bKF4schvGuzI3CULyLzbHOJV5NBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: native-modularity-and-bundle-composition
title: Native Modularity and Bundle Composition
description: Align selectable capabilities with native dependencies, state ownership and independently evolving integration packages
entry_type: design
version: "0.2.1"
status: discussion
```

# Native Modularity and Bundle Composition

This is the native dependency and service/state composition owner beneath the
[platform direction](platform-direction-and-delivery.md). Restored to this
durable location from the preserved 2026-10-05 worktree on 2026-10-06. Source
observations below retain their dates; they are not a fresh completion claim.

## Direction and scope

RyeOS should let a supported node composition depend on the capabilities it
actually needs. Shared native foundations should remain usable without optional
service implementations. Ordinary product behavior should be able to evolve in
admitted data or interpreted packages while reusing installed native binaries.

Bundle selection, Rust dependency selection and execution selection currently
provide different boundaries. A bundle can be absent or disabled while its
supporting native implementation remains an unconditional dependency of shared
crates. This note proposes aligning those boundaries where the capability can
genuinely be optional. It does not define a new minimal product, prescribe a
crate per bundle, or authorize a repository-wide split.

This remains a design owner, not a repository-wide refactor allocation. Native
modularity and language-neutral application development now support the common
platform milestone. Preserve active integration and qualification ownership;
bounded dependency work and interpreted authoring can proceed as separately
selected slices. Neither requires finishing every ambition of the other.

## Existing demand and priority

Native modularity already has concrete consumers. The TUI needs shared client
transport; basic parsers need their own build targets; vault path conventions
currently introduce an engine dependency. The user also needs core-only use to
avoid implementation required exclusively by standard and reports that Rust
compilation is already slowing development on the current machine. Establishing
which features truly are standard-only remains part of the ownership audit;
the demand does not establish that a minimal composition already works.

This is sufficient justification to scope a near-term engineering effort.
It need not wait for a new adapter, a hypothetical deployment or a complete
minimal-node design. The October continuation has selected a bounded basic-parser
dependency reduction; its source/build/qualification evidence belongs to that
implementation's records. Recheck the remaining inventory before choosing the
next extraction. The interpreted-adapter path does not block native cleanup,
and this note does not claim broad app/state separation has been delivered.

The open question is which extraction gives a useful benefit and what correctness
requirements it exposes. A direct dependency removal may leave the same crate
reachable transitively; reduced dependencies do not by themselves prove a faster
build. Measure the actual targets and development loop. The risk of excessive
abstraction should constrain the implementation to existing consumers and their
needs, rather than defer an already observed problem.

## What optional means

| Property | Required outcome |
| --- | --- |
| Optional to install | Its bundle and implementation artifacts can be absent |
| Optional to activate | Its services need not run or acquire authority |
| Optional to compile | Building another supported capability does not require its implementation |
| Independently changeable | Product or service changes avoid rebuilding unrelated foundations |
| Safely removable | Removal preserves retained history and handles outstanding obligations |

These properties are separate. Disabled service registration does not remove
compile dependencies. A separate crate may still be included in every daemon.
An interpreted implementation still needs its admitted runtime and native host
capabilities. Each supported composition must state which properties it offers.

## Source observations and limits

The following observations come from current-checkout source inspection on
2026-10-03. They are candidates for dependency analysis, not a classification of
every module into core or standard. Active implementation trees may differ.
No compilation timing, binary-size measurement or minimal-build proof was run.

Paths below are repository-relative:

| Surface | Observed dependency | Question for extraction |
| --- | --- | --- |
| `crates/daemon/ryeos-app` | Unconditional engine, scheduler, runtime, state, graph-definition and external-execution dependencies; `AppState` directly carries many services | Which services belong in node composition, and which narrow capabilities must foundations expose? |
| `crates/state/ryeos-state` | Shared persistence plus execution, external-content, session and recovery records | Which records are shared execution foundations, and which interpretation/projections can have domain owners? |
| `crates/state/ryeos-vault/src/paths.rs` | Source uses `ryeos_engine::AI_DIR`; engine is a declared dependency | Can shared path conventions or supplied paths remove the implementation dependency? |
| `crates/clients/terminal/src/transport/daemon.rs` | Imports CLI transport, signing, discovery and errors; CLI depends on app/API and other implementation crates | Can CLI and TUI share a smaller client library? |
| `crates/tools/handler-bins` | Basic parsers and graph/directive handlers share a package and unconditional library modules | Can simple handler binaries build without unrelated domain handlers? |
| `crates/engine/ryeos-runtime` | Callback/wire/client helpers coexist with resolution, loaders and authorization; depends on engine/state | Which runtime-facing contracts can be consumed independently of host implementation? |
| `crates/kinds/graph-definition` and `directive-definition` | Definition consumers inherit broader runtime dependencies; graph also depends on engine/bundle | Can data contracts and pure validation avoid loading/execution implementation dependencies? |
| `crates/daemon/ryeos-api` | Shared API infrastructure and domain service handlers assembled in one crate | Which routes belong to separately selected services, and which transport/auth rules are common? |

There are semantic joins as well as manifest edges: `ryeos-app/src/state_store.rs`
calls graph-specific result projection. Such a join needs a replacement owner
and contract, not only moving files. Existing `ExtensionState` supports typed
service-state injection but does not remove concrete crate dependencies by
itself. Narrower surfaces already exist, including `ryeos-expression`,
`ryeos-client-base`, isolation and external-execution contracts.

Existing service descriptors and registry assembly already provide wiring
mechanisms; reuse and narrow them rather than inventing a second registry.
State's `object_closure/contracts.rs` already centralizes current object decoding
and outbound references. Preserve that canonical authority when separating
implementations; extensible registration is an open design, not existing support.

A manifest dependency establishes build coupling, not that all functions appear
in the linked executable, execute at startup or acquire authority. Broad
dependencies are expected at a complete daemon's composition entry point. The
problem is consumers importing that composition to obtain small shared pieces.

## Define foundations through guarantees

Candidate shared responsibilities include admitted identity, authority checks,
execution/resource ownership, durable publication, integrity and recovery.
Their exact minimal implementation remains to be established through supported
use cases. Do not label every existing thread, provider, graph or scheduler
mechanism foundational merely because it currently lives in a shared crate.

A service that supplies scheduling can own its schedules and timer behavior
while invoking an execution interface under selected authority. Execution need
not own that service's database simply because scheduled work is one caller.
Similarly, a graph implementation can own result interpretation while shared
state retains the authenticated result and its declared references.

Bundle placement alone does not determine ownership. Standard may be the first
consumer of a genuinely shared mechanism. Ask whether a supported composition
can operate correctly without a capability and which guarantees would be lost.
Missing required native support must produce an explicit unavailable/refused
capability, never silently select weaker execution behavior.

## Dependency direction and composition

Desired conceptual shape; these are responsibilities, not proposed crate names:

```text
                   Selected node composition
                    /         |          \
          Native services   Runtimes   Product integrations
                    \         |          /
                     Narrow shared contracts
                              |
                 Execution and state foundations
```

The composition owns service selection and wiring. Shared foundations expose
specific operations and enforce their own guarantees. Services depend on those
interfaces; foundations should not import every service's implementation.
Contracts may be smaller and lower-level than their implementation owners.
This diagram is conceptual, not a claim that every component uses a single
contract crate or shares an address space.

Prefer explicit service inputs over giving every consumer all of `AppState`.
Selected interfaces still enforce authorization; a Rust trait or state handle
alone is not a security boundary. Trusted in-process services share the daemon's
process privileges. Less-trusted code needs the existing appropriate managed
execution and capability controls. Registration must preserve required ordering,
startup failure behavior, shutdown and recovery ownership.

Keep canonical contract types and validators with clear owners. Extracting a
type must not produce duplicate definitions or independent interpretations of
the same authority rule. Sharing an interface does not require sharing every
implementation dependency.

## Durable state and removal

State separation must preserve unresolved remote allocations, historical
evidence, retained artifacts and cleanup obligations when a bundle is absent.

| Responsibility | Intended owner |
| --- | --- |
| Store, authenticate, retain and traverse references | Shared state mechanisms under explicit object contracts |
| Interpret product records and build domain projections | Selected domain implementation |
| Permit removal or require reconciliation of outstanding work | Actual operation/lifecycle owner and policy |

Opaque JSON alone is insufficient. Shared state needs reliable reference,
retention, identity and bounds information without understanding every product's
event vocabulary. Any extensible object contract must define who validates those
properties and how unknown/unavailable implementations are handled. Preserve or
refuse safely when required interpretation is unavailable; never guess that an
unknown object has no references or that a missing service has no obligations.
References and retention metadata must derive from canonical validated decoding
or equally authoritative metadata bound to the exact object identity and rooted
under existing atomic publication rules. Before making an interpreter optional,
retain a validated reference extractor or an independently validated envelope.
Untrusted product-supplied reference lists cannot control garbage collection.

Projection ownership can move without moving the authority of underlying facts.
Schema migration, rebuild, atomic publication and cross-domain transactions need
explicit owners. Splitting crates must not turn one required atomic operation
into independently failing writes or create competing schema migration owners.

Distinguish disabling new work from removing the implementation needed to finish
old work. Unresolved operations may require retaining their exact historical
runtime and checker until reconciliation completes. Upgrades must not silently
substitute new semantics for old evidence. Physical purge, service deactivation,
history readability and absence of live obligations are different claims.
Refuse removal unless obligations are discharged or an admitted compatible
recovery composition retains their durable owner and executable recovery path.
Keeping old bytes alone does not establish that path.

## Native modularity and language-neutral integrations

Three boundaries must be addressed together:

1. Bundle selection chooses definitions, capabilities and artifacts.
2. Compilation selection determines which native implementations must build.
3. Execution selection determines which admitted code runs with which authority.

| Change | Expected home when existing contracts suffice |
| --- | --- |
| Another supported configuration | Signed bundle data |
| New product behavior | Admitted implementation package |
| Optional native capability | Independently owned native service |
| New shared execution guarantee | Shared foundation |
| Different supported combination | Node composition |

An initial implementation can use statically assembled native services and
existing managed processes for interpreted packages. Dynamic native loading,
arbitrary plugin discovery and a new service framework are not prerequisites.
Cargo features may help select a finite supported composition, but an arbitrary
feature matrix is not a product plan. Selection must account for transitive
dependencies and the actual build invocation. A full-workspace build still
builds selected workspace members even when a minimal consumer does not need
them; independently usable build/release targets matter.

Changing statically linked native services produces a new daemon/substrate
identity and follows its release and compatibility requirements. Bundle admission
must check required native capabilities; ordinary activation cannot supply a
native capability absent from that composition. This proposal does not widen
the existing publication transaction: its Core keep-only policy, v1 prohibition
on state migration and stopped/drained-node activation requirements remain
operative until a separately qualified transition changes them.

No promise is made to eliminate relinking the complete daemon after a linked
native service changes. The benefit is smaller dependency closures and reusable
unaffected foundations; interpreted product edits have the stronger goal of
unchanged installed native binaries. TypeScript transpilation and native
language dependencies remain separate costs.

## Development loop and trade-offs

Authors should first identify a capability's owner and use an existing contract.
If a new contract is necessary, define the smallest coherent interface with its
failure and evidence rules. Changes moving into widely shared foundations need
a concrete justification; repeated demand for a type may justify a narrow
contract library rather than adding more implementation to app or engine.

Explicit boundaries improve independent development, supported minimal installs,
dependency review and the path to third-party integrations. Costs include more
interface design, lifecycle wiring, compatibility work and recovery reasoning.
Excessively fragmented crates increase navigation and maintenance costs. Group
by coherent ownership and independent change, not by file count. Measure compile
effects rather than assuming more crates automatically make builds faster.

A local authoring harness and reusable runtime dependencies should keep product
iteration short. Packaging, signing and qualification must remain understandable;
moving code out of Rust must not replace compilation delay with unnecessary
full-system packaging for each edit.

## Staging and acceptance

Start with the consumers and builds already needed:

1. Name the actual core-related, parser and client build targets and ordinary
   edit/build workflows, including selected features and packaging steps.
2. Trace unnecessary direct and transitive dependencies, preserving necessary
   capability and authority contracts. Record a reproducible baseline under
   an agreed resource allocation when measurement is scheduled.
3. Select one coherent extraction that removes a meaningful dependency closure
   for an existing consumer. Choose by expected benefit, coupling and correctness
   risk; the vault constant is an example, not an automatic first priority.
4. Establish the before/after dependency graph, preserved behavior and actual
   build/iteration effects. Record daemon relinking and packaging costs separately.
5. Use that evidence and the ownership inventory to scope deeper app/state
   separation. Do not expand the first extraction into a whole-platform redesign.

For composition-level work, define two real inventories: a candidate minimal supported
core and the current full node. The minimal composition's useful operation and
guarantees must be specified before claiming standard-only dependencies can be
removed. Record bundle definitions, native implementations, contracts, state,
startup/recovery services and historical obligations for both compositions.

A bounded dependency cleanup with an existing real consumer, such as extracting
client transport or removing a path-constant dependency, need not wait for this
full inventory. It must preserve current behavior and demonstrate its own
dependency benefit; it does not establish a new minimal supported node.
Require the full inventory before proposing a new composition or optional
state/service removal.

For each suspicious dependency, record its exact source imports, whether it
serves a type or implementation, the responsible owner, transitive closure and
the supported consumer that would benefit. Separate easy dependency cleanup
from deeper authority/state changes. Do not select a wholesale extraction based
only on the manifest inventory in this note.

Select one bounded extraction with a real consumer and a clear before/after
dependency graph. Preserve canonical semantics and delivery ownership. A selected
procedural integration can independently drive interpreted launch support as
described in the related note; migrating existing adapters is not required.
No implementation allocation, build or migration is authorized by this design.

The first extraction has its own scoped acceptance: a real consumer loses the
unnecessary dependency without changing required behavior, and measured costs
and limits are reported. It need not deliver every longer-term outcome below.

Across the subsequent native and interpreted work, ultimately demonstrate:

1. A defined minimal composition builds and performs its supported operation
   without implementations classified as optional for that composition.
2. Changing an optional native service leaves unrelated foundations reusable;
   report actual recompilation and required daemon relinking separately.
3. Two successive ordinary interpreted product changes admit and execute using
   unchanged RyeOS native binaries.
4. Missing support is refused explicitly; deactivation/removal preserves required
   history, artifact reachability and unresolved-operation recovery.

Measure clean and incremental build scopes/times, build configuration, affected
artifacts, edit-to-feedback and edit-to-admitted-run time. A dependency graph is
source evidence; successful builds, behavior and recovery require their own
evidence. No such acceptance is claimed here.

## Open decisions and related work

- What useful operation defines the minimal supported core composition?
- Which existing standard consumers require shared native mechanisms?
- Which contract extractions remove meaningful transitive dependencies?
- How are optional state interpreters, references and recovery prerequisites
  admitted and retained without a second persistence authority?
- Which small set of compositions merits maintained build/release coverage?
- Which first extraction offers useful isolation without reopening active cuts?

[Language-neutral integration adapters](language-neutral-integration-adapters.md)
owns the product-language direction and its trust/launch constraints. This note
adds native dependency and durable-state composition; neither alone solves all
development costs.

[Native bundle publication and node composition](native-bundle-publication-and-node-composition.md)
owns the related artifact/publication direction. Reuse its owners rather than
inventing a second deployment or service-selection authority.

[Generation state capsules](generation-state-capsules.md) and
[Nested execution ownership and evidence](nested-execution-ownership-and-evidence.md)
provide related state and execution context; this discussion does not change
their operating contracts.
