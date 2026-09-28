<!-- ryeos:signed:2026-09-22T02:52:14Z:4afff42150c42e6febc259d466b5d9a760ade78b3175434d9dde588c54b9390e:wgAQPb471T44tDk4d5jGNdctTIH/QZP0/82MVsw518kaN/mDGDLQBmexJQCy1+sRSrxsSAfr1jY03SFxA9oNBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: native-bundle-publication-and-node-composition
title: Native Bundle Publication and Node Composition
description: Scheduled direction for independently publishing exact RyeOS bundle generations and composing nodes without rebuilding the substrate image
entry_type: design
version: "0.5.0"
status: implementation_under_qualification
```

# Native bundle publication and node composition

## Status

Implementation is under qualification, not merely scheduled design. Publication
contracts, catalog handlers, calibration and bundle-set transaction source,
release-authority initialization selection and release Graphs exist in the
current development tree. Their existence is not end-to-end acceptance.
The 2026-09-22 checkpoint has a verified and imported corrected Stage-0
compiler artifact and a retained platform product witness. Independent live
platform qualification remains open; it does not yet prove this campaign published,
transferred and activated a bundle on another node without an image rebuild.
The image/tag release runbook remains the established shipping path until the
native path passes its acceptance gates.

Continue from the dated checkpoint in
[`source-local-bundle-development.md`](../development/source-local-bundle-development.md)
and the ownership rules in
[`development-operation-ownership.md`](../development/development-operation-ownership.md).
That checkpoint owns concrete local evidence and pending steps; this document
owns the architectural direction. The external Stage-0 publisher seeds missing
compiler bytes once per selected toolchain change. It is neither the normal
bundle publication workflow nor a reason to rebuild the serving substrate for
each bundle update. Complete native development execution remains a longer-term
direction, and bootstrap exceptions must not grow into a second release system.

Current calibration requires qualified Python, platform and Cargo-vendor
products. This initial authority setup is distinct from recurring updates:
data-only production uses the Python-only graph; binary-bearing production uses
the selected build targets and retained compiler/dependency products. Reuse
calibration while its measured identities and policy remain admissible; changes
that invalidate the measurement require recalibration. The current completion
campaign proves both successor classes across nodes, unchanged substrate
identity, pre-commit recovery and refusal of post-execution rollback in v1.
The original acceptance properties below remain mandatory. Full worker development acceptance,
Stage1 reproduction, durable cache optimization and self-hosted CI migration
remain separate work. The dated execution-closure implementation plan owns the
remaining sequence, while this document retains the architectural boundaries.

## Purpose

RyeOS currently rebuilds and publishes complete release images when one
non-core bundle or bundle-owned binary changes. That couples two different
release cadences:

- the substrate image, which carries `ryeosd`, the CLI, bootstrap machinery,
  operating-system/runtime dependencies, and substrate-bound Core content; and
- independently useful bundles, which can often change without changing the
  daemon or container substrate.

The desired system publishes one stable substrate image when the substrate
changes, while bundle generations are built, qualified, signed, and uploaded
independently. A deployed bundle-source node serves a bounded current catalog
and explicitly retained immutable release closures. Other nodes select an exact bundle set, fetch its closure, apply local
admission, and compose their installed generation.

The canonical publication topology crosses an authenticated remote boundary:
the release authority uploads the exact bounded closure through a pinned RyeOS
named remote to a persistent bundle-source node and advances its catalog with
the resulting durable upload session. The normal RyeOS remote client preserves
authenticated node identity; this path does not introduce a parallel bearer
protocol. Sharing a substrate image does not imply sharing a CAS or collapsing
those authorities. A deliberately combined node may use local closure staging
as an explicit optimization, but the normal release Graph and default release
profile do not depend on or receive that capability.

This is also the first concrete release slice of the
[RyeOS-native development platform](ryeos-native-development-platform.md).
GitHub Actions may invoke it during migration, but the release semantics must
belong to RyeOS-native operations and evidence.

## Architectural separation

### Substrate plane

The substrate image contains only what is required to boot and verify a node:

- `ryeosd` and the RyeOS CLI;
- bootstrap verification and installation tooling;
- required operating-system and runtime dependencies;
- an exact substrate/protocol generation declaration; and
- the minimum substrate-bound Core generation required to admit later content.

It is published infrequently through an image or native-archive channel. A
normal bundle update does not rebuild it. Hard-contained, device-specific, or
otherwise security-distinct images may remain separate products where their
packaging is itself part of the isolation or hardware contract.

### Publication plane

An admitted producer creates an exact clean candidate from source, environment,
build operations, and outputs. Because current bundles carry publisher-signed
manifests/items, an isolated publisher transaction verifies and signs that
candidate before its final tree is captured and qualified. The publisher later
authorizes the immutable generation and exact bundle-set/catalog successors.
The bundle-source node verifies and retains the published closure but cannot
forge publisher authority.

The bundle-source node stores generations without installing or activating all of
them. Catalog presence is not runtime authority.

### Consumer plane

A consumer selects exact publisher-authorized generation-publication and,
where applicable, curated set-publication coordinates, not an ambient `latest`.
It fetches the bounded closure, verifies content and
publisher evidence locally, authors or accepts an exact node-bundle selection,
validates the prospective installed graph, and activates that selection through
a crash-recoverable transaction. Node policy remains separately owned by the
node.

The complete deployment coordinate is:

```text
substrate image digest
+ deployment-authorized node-bundle-selection hash/receipt
    -> exact generation-publication attestations
    -> optional publisher-curated set-publication attestation and subject set
+ consuming bundle-publication policy-section digest
+ node-policy generation
```

Those identities may advance independently only where their declared
compatibility permits it.

## Roles and authority

The roles are logically separate even when an initial deployment colocates
some of them:

| Role | Owns | Must not infer |
| --- | --- | --- |
| Source/project owner | exact source generation or accepted candidate | release fitness |
| Development/build node | admitted build and check execution | publication or deployment authority |
| Qualification | scoped evidence about exact output | publisher or consumer trust |
| Publisher | authorization of an exact generation/set transition | successful activation |
| Bundle-source node | verified storage, catalog availability, closure serving | authority to alter signed publisher content |
| Consumer node | local trust, admission, staging, installed set | producer or publisher authority |
| Deployment authority | selection and activation of an exact admitted coordinate | permission to rewrite build evidence |

The publisher private key does not live on a general development worker or
bundle-source node. Publication should use a narrowly scoped publisher service,
operator action, or delegated capability that independently verifies and signs
only the exact qualified subject selected by policy. Build completion must not
grant a generic sign-any-hash operation.

Transport admission is separate from publisher custody. Each catalog explicitly
lists `authorized_uploaders` as node fingerprints; membership permits transport
only alongside the required service capabilities. The publisher is not implicitly
an uploader. Upload sessions retain their actual authenticated owner, while every
artifact signature must independently satisfy the pinned publisher policy. No
release node needs the publisher private key to upload a signed release.

Bundle-source and consumer nodes independently admit publishers through a
`bundle_publication` section in their existing atomic, operator-signed node-
policy generation. It binds catalog namespace, fingerprint, accepted
claim/policy, trust epoch, and any delegation. This avoids a parallel policy
authority. The current implementation binds the complete section digest in
release evidence, so release, source, consumer, and constrained-publisher policy
must use the same section. Independent differing sections require a future
explicit policy-equivalence or delegation contract. General trust-store
membership alone does not authorize a key for every catalog. Publication must
prove the snapshot publisher, namespace owner, publication-attestation issuer,
and selected source section agree or carry an explicit admitted delegation;
consumers repeat the equivalent check under their own section. Receipts retain
both the section digest and whole node-policy generation digest.

The current key-lifecycle design has no rotation or succession contract.
Production therefore requires at least a static trust epoch and explicit
compromise freeze/clean-cut recovery procedure, without claiming automatic
continuity for old releases after revocation. Under the current fail-closed
model, revocation may invalidate historical recovery. Recording the old policy
digest preserves evidence of the prior decision; it does not preserve present
authority to activate or recover under that policy.

## Durable identities

The first implementation should prove whether existing retained-product and
attestation objects can carry each claim before adding new kinds. The release
protocol nevertheless needs four explicit semantic identities.

### Bundle generation

One immutable generation identifies:

- bundle name and authored version;
- exact portable tree/content-manifest hash;
- file, byte, path, mode, and link bounds;
- target triple or an explicit portable target;
- build profile;
- required substrate and daemon protocol generations;
- the existing accepted product result and exact selected product
  identity/witness;
- a typed publisher-materialization result binding that unsigned candidate and
  signer to the exact final signed-tree manifest;
- an exact source-snapshot edge only when release-retention policy requires it;
- qualification, provenance, notice, and SBOM evidence where required; and
- publisher attestation over the exact subject.

The generated in-bundle manifest remains part of the tree. Its human version is
not the generation identity. The CAS root and publisher evidence are.

The existing external-content manifest contract should be reused for canonical
paths, normalized executable modes, internal links, byte counts, and blob
edges unless implementation proves it cannot faithfully represent a bundle.
The current live-directory `bundle/export` response is outside this protocol.
Native publication never converts or admits it as a generation, and no
compatibility adapter is part of this design.

Bundle contracts have three layers. The wire decoder validates only local
syntax, ordering, vocabularies, and bounds. A meaning-blind closure contract
declares typed CAS edges. A context-aware semantic verifier resolves those
edges and policy to produce verified generation/set/catalog values required by
publication and admission. State supplies the registry mechanism but does not
own or interpret bundle domain semantics; the application composes provider
and bundle contracts for daemon and maintenance entrypoints.

The generation reuses `ProductBuildAcceptedResult` rather than inventing a
second weaker provenance description. The general build worker produces an
unsigned exact candidate. A constrained publisher transaction independently
materializes and signs the final in-tree manifest/items; the final signed tree
is then captured and qualified before generation finalization and release
attestation. Its typed publisher-materialization result binds the input
manifest and accepted product, output manifest, signer/tool identity, and a
closed allowed signature-only mutation contract. Verification strips or
normalizes only those permitted signature additions and proves every other
path, byte, and mode equal, so the transformation is not an unaccounted
provenance gap. V1 requires the in-tree signer and release-attestation signer
to be the same exact publisher fingerprint. Separate signers require a later
explicit two-key policy contract.

Every generation requires the accepted product, selected witness, and
publisher-materialization result above. There is no alternate archive/import
lineage, compatibility schema, or evidence-light publication path. Unknown or
older generation schemas fail closed.

V1 release attestations are non-expiring and remain subject to current local
publisher policy and revocation. Their signed `issued_at` is an issuer claim,
not an independent timestamp or freshness proof. Expiring release authority
requires a later explicit offline recovery and rollback policy.

### Bundle set

One immutable bundle set identifies:

- an exact, closed mapping from bundle name to generation identity;
- target and substrate protocol requirements;
- set-level compatibility or migration requirements.

For v1 it is the complete installed bundle payload inventory and accepts only
`migration_requirement: none`. It includes a substrate-bound Core entry that is
keep-only during normal bundle updates. Core may be changed only by an exact
substrate transition that authorizes that old/new identity.

An exact set is immutable and context-free. A publisher may attest a curated,
tested set, but catalog object presence does not create set membership and a
consumer cannot claim publisher approval for a combination the publisher did
not attest. Bundle payload selection remains separate from node-policy
selection: a publisher-authored bootstrap profile may declare one exact bundle
inventory, but the consuming node creates or retains its own admitted policy
generation.

### Node bundle selection

The deployment authority authors the exact desired node bundle selection. It
references publisher-authorized generation attestations and may also reference
one publisher-curated set attestation. A custom selection without a curated-set
attestation is valid only when local policy permits it and local prospective
admission accepts the complete combination; its receipt must state plainly that
the publisher authorized the component generations, not that exact set.

The node-bundle selection is separate from node policy. It records the exact
payload generation desired for one deployment, the consuming bundle-
publication policy-section and whole node-policy generation digests,
compatibility/migration decisions, and any curated-set evidence.
Deployment authority signs or otherwise durably authorizes that exact subject.
The first implementation adopts publisher-curated sets only. Custom consumer
composition follows after the stopped-node set transaction is qualified.

### Catalog publication

A catalog snapshot maps bounded per-bundle channels to exact generation-
publication attestations and optional curated-set channels to exact set-
publication attestations. A separate catalog-publication subject identifies
that snapshot, the exact previous catalog-publication attestation, and its
successor sequence. The publisher attests the publication subject. A
bundle-source node retains the attestation under a generic signed head so
existing reachability and garbage collection preserve the current closure.

The predecessor is a compare-and-swap transition coordinate, not a content
dependency of the new snapshot. Closure traversal must not recursively retain
unbounded catalog history through that field. Immutable bundle generations and
set publications receive explicit retention roots; retained catalog checkpoints
or rollback windows are separate bounded operator policy. This lets old catalog
transition records become collectable without deleting still-published bundles.

The bundle-source node's local head is an availability root, not sufficient
release authority. A consumer verifies and pins exact generation-publication
attestations and, when selected, a curated set-publication attestation whose
subject is the bundle-set hash. A raw generation/set hash proves content
identity but cannot prove or discover publisher authorization. Deployment then
authorizes the exact node-bundle selection over those releases. Convenience
channel resolution must produce complete publication coordinates before that
decision. Previously observed channel state may not move backward without an
explicit rollback decision.

Signatures alone do not prove freshness to a first-contact consumer. A new
consumer following a friendly channel needs an operator-supplied exact set-
publication coordinate or catalog checkpoint, another already-trusted
observation, or an explicitly
accepted first-contact policy. Without one of those, a compromised source can
serve an older valid publisher snapshot even though it cannot forge a new one.

Publisher signatures also do not prevent the publisher from signing sibling
successors for the same predecessor. The publisher service serializes against
its admitted expected predecessor to prevent accidental equivocation; exact
deployment coordinates and consumer checkpoints make observed forks visible.
A globally consistent transparency service is not part of the first slice.

## Native production lifecycle

The intended producer is a RyeOS development node:

```text
exact source closure
  -> affected-bundle selection
  -> admitted build environment
  -> build all payloads owned by each selected bundle
  -> accepted clean unsigned product
  -> constrained publisher materialization and in-tree signing
  -> capture final signed bundle tree
  -> bundle validation and focused checks over that final tree
  -> independent or policy-required qualification
  -> generation finalization and publisher release authorization
  -> upload missing CAS objects
  -> compare-and-swap catalog publication
```

Tools own individual operations. A Graph owns sequencing, fan-out, failure, and
lineage. The content store owns byte identity. Qualification owns scoped claims.
The publisher owns the authority transition to a reusable release.

The constrained publisher service is a prerequisite authority boundary, not
merely the final node of the build Graph. It exposes no generic sign-any-hash
operation and serializes catalog authorization against the exact admitted
predecessor.

The first implementation may build from a Git checkout, but it must capture an
exact source coordinate or closure before claiming a release result. A mutable
checkout is an input workspace, not durable release identity.

The implemented native-build admission path now treats the checkout only as
source provenance. After the constrained publisher authors the exact
per-release recipe, the release node re-materializes and re-hashes the admitted
Git archive, installs that recipe at the fixed project-overlay Config identity,
captures the augmented project as a pinned generation, verifies that the
snapshot overlay resolves the publisher-authored raw digest, and executes from
that generation. The post-sign transformation follows the same model: the
publisher authors an exact signed-capture recipe bound to the fixed trusted-
bundle qualification policy, a second admitted producer captures the signed
tree, and the parameter-free qualifier derives its subject facts from that
admitted signed-tree realization. The generation retains both immutable
accepted results, the publisher materialization, the signed witness, and its
qualification so authorization can verify the complete build-to-release chain
without mutating the original accepted result.

For an affected bundle, the first implementation rebuilds every binary payload
owned by that bundle. It must not preserve unselected output from an ambient
target directory. Later optimization may reuse outputs only from a verified
previous generation named explicitly in the build request.

## GitHub migration

GitHub is a transitional caller and mirror:

1. The current image workflow remains the recovery path.
2. Package, verify, qualify, publish, resolve, fetch, and apply become ordinary
   RyeOS-owned operations with local/CLI entrypoints.
3. A thin GitHub workflow may select an immutable source commit and invoke the
   native release operation. It contains no unique ownership map, package
   assembly, compatibility, signing, or catalog-transition logic.
4. A RyeOS-triggered Graph and the GitHub-triggered adapter are qualified over
   identical inputs.
5. The RyeOS development node becomes the primary executor.
6. GitHub remains optional source/release mirroring and bootstrap recovery.

Eventually substrate-image production can also run through an admitted RyeOS
Graph and publish to GHCR or another registry. Moving the image build does not
remove the independently recoverable bootstrap image and trusted publisher-key
material needed to start a clean node.

## Bundle-source-node behavior

The bundle-source deployment uses durable node state. Image startup may seed an
empty or exact initial catalog only when no catalog exists. Redeploying the
substrate image must not overwrite a newer persistent catalog head.

The service surface should remain small:

- existing authenticated, bounded `objects/closure/describe` and
  `objects/closure/get` operations, fetching one generation closure at a time
  in the first slice;
- a catalog-specific durable upload session reusing the current CAS staging
  machinery without pretending the upload is a project-head publication;
- exact catalog resolution and inspection;
- publisher-authorized catalog publication; and
- operator diagnostics, retention, and repair.

Normal publication always uses the authenticated named-remote upload route
from the release authority to the bundle-source node, followed by catalog publication against
the exact expected predecessor. A local-stage route may exist for a dedicated
combined topology where both roles intentionally share a CAS. It is not part
of the default release-authority capability set and must never silently replace
a failed or missing remote transport configuration.

The current `objects/put` session is bound to a principal-scoped project HEAD
and cannot be reused unchanged. The catalog path needs its own typed durable
publication key and upload route, while sharing the bounded chunking, hashing,
staging-root, recovery, and lease machinery. Publication verifies the complete
bounded closure before advancing the visible catalog head and consuming that
session. Partial uploads remain protected only by their bounded staging lease,
then become collectable. Concurrent publishers use expected-predecessor
compare-and-swap rather than last-writer wins.

Read and publish capabilities are separate. A read client cannot publish. A
publisher cannot mutate node policy merely because it can submit a catalog
transition.

The complete verified closure remains pinned from publication validation
through durable head commit. The upload session is consumed only afterward.
Retry after an ambiguous response is idempotent when the exact target is already
current. A current head with a missing or corrupt closure is degraded and
repair-required; startup never replaces it with genesis or infers rollback.

The durable catalog upload/publication key also binds the exact source
`bundle_publication` policy-section digest and whole node-policy generation
digest used for admission. Before normal or recovered head commit, those
digests must still be current. A successful semantic re-admission under a new
generation must mint/rebind a durable key/session or append a durable admission
record with the new digests before commit; stale recorded authority cannot
continue through an in-memory recheck. Revocation or failed re-admission rejects
the session and preserves it only for bounded diagnosis/cleanup.

Explicit generic-head namespaces own the current catalog, bounded catalog
checkpoints, immutable generation/set publications, consumer active selection,
bounded previous/rollback selections, and durable newest-seen anti-replay
checkpoints. Recovery may retry an exact validated commit or preserve an
ambiguous admitted session for diagnosis; it cannot infer a successor, skip a
predecessor, or use genesis seeding as repair.

The general image must carry the audited union of OS/runtime dependencies for
the profiles it replaces. Real containment, accelerator, or hardware boundaries
remain separate variants. Entrypoint initialization becomes seed-only-if-
absent, and restart qualification proves that baked bootstrap content never
overwrites a newer persistent catalog or active selection.

## Consumer composition and activation

Fetching and activation are separate:

1. Resolve or receive an exact set-publication attestation and its subject set.
2. Fetch and verify publication metadata/evidence for every entry and each
   non-Core generation closure through bounded closure describe/get operations;
   Core is verified against the substrate-seeded local identity and remains
   `keep`. Whole-set transfer waits for a chunked large-object protocol.
3. Materialize every non-Core candidate bundle into hidden immutable staging.
4. Verify publisher evidence, target, substrate protocol, manifests, and tree
   identity.
5. Build one `ReconcileExactSet` prospective plan over complete target and
   installed inventories, with explicit add/replace/remove/keep actions.
6. Run registry, kind, protocol, route, command, runtime-authority, isolation,
   and native-executor admission.
7. Require deployment authority over the exact node-bundle selection.
8. Commit through one set-level durable transaction with a defined commit point.
9. Record the active selection and preserve or explicitly update node policy.

Fetch creates a bounded consumer-local lease covering the exact publication
closure through plan and apply. The active selection and each explicitly
retained previous selection receive durable local roots. Source-side retention
does not protect a consumer from its own concurrent garbage collection.

The existing per-bundle transaction and replacement path are useful primitives
but do not make a multi-bundle set atomic. The first production activation path
should require a stopped or drained node. A set-level journal must recover all
tree and registration changes before normal daemon startup. Live multi-bundle
replacement is a later capability, not an assumption hidden in the first API.

The existing operator-signed whole-node init completion evolves to bind the
active node-bundle selection/publication and remains the externally startable
commit point. A node-signed active-selection head is not enough to supersede
that operator authority. The exact valid completion content—not a journal
phase—is the recovery decision: matching new completion finishes forward,
matching retained old completion restores old, absence restores old only when
the journal proves precommit, and corrupt/unrelated content fails closed for
operator repair. The journal retains exact old/new completion hashes plus
transaction and selection identities.

Daemon bootstrap performs only narrowly authorized set-journal recovery before
ordinary init-completion verification, under the state and bundle-registry
locks. It may restore the exact old set/completion or finish the exact
deployment-authorized new pair; it cannot author another selection. Completion
verification must first move to pinned operator public material, with the
operator private key outside the daemon-readable app root and available only to
the stopped/offline apply boundary.

Before the operator-signed completion commit, recovery restores the old
complete filesystem/registration generation and old completion; at or after it,
recovery completes forward to the new pair. Only then may ordinary init
verification admit engine composition. No candidate bundle code or migration
runs before commit, and v1 accepts no migration requirement other than `none`.
That value does not prove backward state compatibility. V1 therefore refuses
post-execution rollback; adding it requires a typed state-effect/rollback-safety
contract and a new deployment decision. Those transitions remain connected to
reflexive deployment.

## Compatibility boundary

Bundle generations should name exact RyeOS protocol generations rather than
open-ended semantic-version guesses. At minimum admission checks:

- substrate/daemon protocol generation;
- target architecture and ABI where native binaries exist;
- build profile;
- bundle manifest format and kind-schema generation;
- required isolation/runtime adapter protocols; and
- any explicit state migration prerequisite.

Core is substrate-bound and keep-only in normal bundle updates. Its exact set
entry must match the selected image, and normal catalog apply cannot fetch or
replace it independently. A Core transition requires an exact substrate
coordinate authorizing the old/new identity until a narrower independent
compatibility contract is proved. The economic win comes first from data-only
bundles and non-core binary bundles.

## Retention, mirroring, and recovery

Published generations remain immutable. Each admitted generation and set
publication receives an explicit immutable retention root; the moving catalog
head retains only its current bounded publication closure. Retention policy may
later remove selected old generation/set roots, making their otherwise
unreferenced closures collectable, but never by deleting files from a currently
published tree or by pretending an unbounded predecessor chain is bounded.

A mirror verifies and stores the same publisher-authored closure. It does not
mint a new release merely because it serves the bytes. Exact generation- and
set-publication attestation coordinates allow consumers to change mirrors
without changing the selected release.

Bootstrap recovery must preserve:

- one known substrate image or native archive;
- the current static publisher trust epoch and explicit compromise/replacement
  procedure; succession evidence only after key lifecycle defines it;
- an export or mirror of required catalog closures; and
- an operator path for restoring the bundle-source head without granting the
  bundle-source node publisher authority.

The native catalog starts from a freshly authorized genesis publication created
through this protocol. Existing release archives are not imported or converted
into native generations. During rollout they remain external recovery artifacts
under the current release process, outside the native catalog's authority and
lineage.

## Delivery stages

### Stage 1 — immutable publication contracts

- Prove/reuse the canonical portable tree representation.
- Add the minimum typed generation, set, node-bundle-selection, catalog, and
  evidence contracts.
- Register complete closure edges and bounded validators.
- Provide local package and verification operations.

### Stage 2 — bundle-source node

- Add publisher-authorized catalog publication and exact read/resolve APIs.
- Reuse bounded per-generation closure transfer and generic signed-head
  retention.
- Add durable bundle-source bootstrap, restart, diagnostics, and repair tests.
- Create a fresh catalog genesis and first native data-bundle publication
  through the same production contracts used for every successor.
- Prove fetch-only verification with one data bundle.

### Stage 3 — constrained publisher service

- Add independent candidate verification and final in-tree manifest/item
  signing.
- Add exact generation, curated-set, and serialized catalog-successor
  authorization.
- Qualify static trust-epoch freeze and clean-cut replacement.

### Stage 4 — exact consumer composition

- Fetch and verify exact curated sets without activation.
- Add `ReconcileExactSet` and complete multi-candidate prospective admission.
- Integrate stopped-node set recovery with operator-signed whole-init
  completion and daemon startup ordering.
- Prove two consumers can intentionally run different retained generations.

### Stage 5 — native producer and temporary GitHub adapter

- Express affected-bundle build, assembly, checks, retention, qualification,
  and publication as RyeOS-native operations and a Graph.
- Publish one data-only bundle, then one non-core native bundle.
- Keep a thin GitHub trigger only where current bootstrap requires it.
- Prove bundle-only publication performs no image build and no daemon build.
- Prove the default Graph transfers the bounded closure to a separately
  deployed bundle-source node; local staging is exercised only by an explicit
  combined-node profile.

### Stage 6 — stable substrate deployment

- Publish and deploy one general substrate image carrying a small bootstrap seed
  closure. Profile selection activates the bundle-source service where needed;
  the persistent catalog is seeded only when absent.
- Move normal deployment coordinates to image digest plus deployment-authorized
  node-bundle selection, consuming bundle-publication policy-section digest,
  and whole node-policy generation.
- Retain current profile images until update, restart, and recovery evidence is
  green; their availability does not itself authorize state rollback.
- Remove redundant image variants only when their remaining differences are
  selection policy rather than real containment or hardware boundaries.

### Stage 7 — RyeOS-primary release execution

- Make the RyeOS development node the primary release executor.
- Project results back to GitHub rather than treating Actions as authority.
- Add native scheduling, review, and release surfaces only from observed need.
- Connect published set selection to reflexive deployment when that owner is
  pulled forward.

These stages are safety and authority gates. The official native producer Graph
does not begin before the publisher signing order and whole-init exact-set
activation contract are qualified.

## Acceptance properties

The first production cut is complete only when:

- the deployed substrate image remains byte-identical across a bundle-only release;
- the default release path uploads to a persistent bundle-source node and does
  not require a shared CAS or local-stage capability;
- bundle-only production does not build or publish an OCI image or compile
  `ryeosd`;
- the new generation retains exact scoped evidence claims about source,
  producer, environment, checks, publisher, target, and content, and each claim
  is accepted only under explicit policy;
- bundle-source-node restart preserves current and retained catalog generations;
- the bundle-source node cannot forge publisher evidence;
- consumers reject missing, partial, malformed, oversized, wrong-target,
  incompatible, unsigned, or wrongly signed closures;
- one consumer can move to the new set while another remains on the old set;
- interrupted upload, catalog advance, staging, and activation recover without
  inferred success;
- crash recovery establishes an installed-set/whole-init-completion pair before
  ordinary startup verification or engine composition;
- normal bundle application cannot replace substrate-bound Core and accepts no
  unimplemented migration requirement;
- pre-commit recovery follows its exact contract and post-execution rollback is
  refused until a typed state-effect/rollback-safety contract exists; and
- the same release operations can be invoked by a RyeOS Graph without relying
  on hidden GitHub workflow behavior.

## Non-goals

- An OCI image per bundle generation.
- Treating OCI registries as the RyeOS bundle protocol.
- Installing every catalog bundle on the bundle-source node.
- Giving a build worker or bundle-source node the publisher private key.
- Floating consumer installation from an unpinned `latest` response.
- Inferring bundle sets from catalog contents.
- Replacing Git source hosting in the first slice.
- General hostile multi-tenant package hosting.
- Live hot replacement before stopped-node set activation is proven.
- Post-execution rollback before a typed state-effect/rollback-safety contract.
- A new universal package/build recipe language or parallel content store.
- Automatic self-publication or self-deployment after successful checks.
