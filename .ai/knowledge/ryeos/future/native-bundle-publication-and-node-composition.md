```yaml
category: ryeos/future
name: native-bundle-publication-and-node-composition
title: Native Bundle Publication and Node Composition
description: Scheduled direction for independently publishing exact RyeOS bundle generations and composing nodes without rebuilding the substrate image
entry_type: design
version: "0.1.0"
status: scheduled_design
```

# Native bundle publication and node composition

## Status

Scheduled design for the next release-distribution slice. The object schemas,
catalog APIs, bundle-set activation transaction, source-node profile, and
RyeOS-native release Graph described here have not landed. The current GHCR
release process remains authoritative until this path passes its acceptance
gates.

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
independently. A deployed bundle-source node serves the complete immutable
catalog. Other nodes select an exact bundle set, fetch its closure, apply local
admission, and compose their installed generation.

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

An admitted producer creates an immutable bundle generation from exact source,
environment, build operations, and outputs. Qualification records scoped claims
about that generation. A separate publisher authorizes an exact generation and
an exact bundle-set successor. The bundle-source node verifies and retains the
published closure but cannot forge publisher authority.

The bundle-source node stores generations without installing or activating all of
them. Catalog presence is not runtime authority.

### Consumer plane

A consumer selects an exact publisher-authorized generation/set coordinate,
not an ambient `latest`. It fetches the bounded closure, verifies content and
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
+ consuming publisher-trust policy generation
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

Bundle-source and consumer nodes independently admit publishers through an
exact local publisher-policy generation signed by their node/operator policy
authority. It binds catalog namespace, fingerprint, accepted claim/policy, and
any delegation. The two policies may legitimately differ. General trust-store
membership alone does not authorize a key for every catalog. Publication must
prove the snapshot publisher, namespace owner, publication-attestation issuer,
and selected source policy agree or carry an explicit admitted delegation;
consumers repeat the equivalent check under their own policy.

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
protocol nevertheless needs three explicit semantic identities.

### Bundle generation

One immutable generation identifies:

- bundle name and authored version;
- exact portable tree/content-manifest hash;
- file, byte, path, mode, and link bounds;
- target triple or an explicit portable target;
- build profile;
- required substrate and daemon protocol generations;
- source generation and producer execution identities;
- qualification, provenance, notice, and SBOM evidence where required; and
- publisher attestation over the exact subject.

The generated in-bundle manifest remains part of the tree. Its human version is
not the generation identity. The CAS root and publisher evidence are.

The existing external-content manifest contract should be reused for canonical
paths, normalized executable modes, internal links, byte counts, and blob
edges unless implementation proves it cannot faithfully represent a bundle.
The current live-directory `bundle/export` response is a migration mechanism,
not the durable generation format.

V1 release attestations are non-expiring and remain subject to current local
publisher policy and revocation. Their signed `issued_at` is an issuer claim,
not an independent timestamp or freshness proof. Expiring release authority
requires a later explicit offline recovery and rollback policy.

### Bundle set

One immutable bundle set identifies:

- an exact, closed mapping from bundle name to generation identity;
- target and substrate protocol requirements;
- set-level compatibility or migration requirements.

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
payload generation desired for one deployment, the consuming publisher-policy
generation, compatibility/migration decisions, and any curated-set evidence.
Deployment authority signs or otherwise durably authorizes that exact subject.
The first implementation adopts publisher-curated sets only. Custom consumer
composition follows after the stopped-node set transaction is qualified.

### Catalog publication

A catalog snapshot maps bounded publisher/channel names to exact set-publication
attestation hashes. A separate catalog-publication subject identifies that
snapshot, the exact previous catalog-publication attestation, and its successor
sequence. The publisher attests the publication subject. A bundle-source node
retains the attestation under a generic signed head so existing reachability and
garbage collection preserve the current closure.

The predecessor is a compare-and-swap transition coordinate, not a content
dependency of the new snapshot. Closure traversal must not recursively retain
unbounded catalog history through that field. Immutable bundle generations and
set publications receive explicit retention roots; retained catalog checkpoints
or rollback windows are separate bounded operator policy. This lets old catalog
transition records become collectable without deleting still-published bundles.

The bundle-source node's local head is an availability root, not sufficient release
authority. A consumer verifies the publisher evidence and pins the exact
set-publication attestation it intends to install; the attestation identifies
its subject bundle-set hash. A raw set hash proves content identity but cannot
prove or discover publisher authorization. Convenience channel resolution must
produce the complete publication coordinate before deployment. Previously
observed channel state may not move backward without an explicit rollback
decision.

Signatures alone do not prove freshness to a first-contact consumer. A new
consumer following a friendly channel needs an operator-supplied exact set or
catalog checkpoint, another already-trusted observation, or an explicitly
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
  -> assemble clean bundle trees
  -> bundle validation and focused checks
  -> retained bundle products
  -> independent or policy-required qualification
  -> publisher authorization
  -> upload missing CAS objects
  -> compare-and-swap catalog publication
```

Tools own individual operations. A Graph owns sequencing, fan-out, failure, and
lineage. The content store owns byte identity. Qualification owns scoped claims.
The publisher owns the authority transition to a reusable release.

The first implementation may build from a Git checkout, but it must capture an
exact source coordinate or closure before claiming a release result. A mutable
checkout is an input workspace, not durable release identity.

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

## Source-node behavior

The bundle-source deployment uses durable node state. Image startup may seed an
empty or exact initial catalog only when no catalog exists. Redeploying the
substrate image must not overwrite a newer persistent catalog head.

The service surface should remain small:

- existing bounded object presence and retrieval operations;
- a catalog-specific durable upload session reusing the current CAS staging
  machinery without pretending the upload is a project-head publication;
- exact catalog resolution and inspection;
- publisher-authorized catalog publication; and
- operator diagnostics, retention, and repair.

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

## Consumer composition and activation

Fetching and activation are separate:

1. Resolve or receive an exact set-publication attestation and its subject set.
2. Fetch and verify the complete object/blob closure under explicit bounds.
3. Materialize every candidate bundle into hidden immutable staging.
4. Verify publisher evidence, target, substrate protocol, manifests, and tree
   identity.
5. Build one prospective plan for the complete post-operation bundle graph.
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

Before the set commit point, recovery restores the old complete filesystem/
registration generation; at or after it, recovery completes forward to the new
generation. No candidate bundle code or migration runs before commit. Rollback
after execution is a new deployment decision, not transaction recovery, and is
not automatically safe across state-schema or irreversible data migrations.
Those transitions require explicit compatibility/migration evidence and remain
connected to reflexive deployment.

## Compatibility boundary

Bundle generations should name exact RyeOS protocol generations rather than
open-ended semantic-version guesses. At minimum admission checks:

- substrate/daemon protocol generation;
- target architecture and ABI where native binaries exist;
- build profile;
- bundle manifest format and kind-schema generation;
- required isolation/runtime adapter protocols; and
- any explicit state migration prerequisite.

Core content that changes the bootstrap verifier, daemon protocol, or state
epoch remains substrate-bound until a narrower independent compatibility
contract is proved. The economic win comes first from data-only bundles and
non-core binary bundles.

## Retention, mirroring, and recovery

Published generations remain immutable. Each admitted generation and set
publication receives an explicit immutable retention root; the moving catalog
head retains only its current bounded publication closure. Retention policy may
later remove selected old generation/set roots, making their otherwise
unreferenced closures collectable, but never by deleting files from a currently
published tree or by pretending an unbounded predecessor chain is bounded.

A mirror verifies and stores the same publisher-authored closure. It does not
mint a new release merely because it serves the bytes. Exact set coordinates
allow consumers to change mirrors without changing the selected release.

Bootstrap recovery must preserve:

- one known substrate image or native archive;
- the current static publisher trust epoch and explicit compromise/replacement
  procedure; succession evidence only after key lifecycle defines it;
- an export or mirror of required catalog closures; and
- an operator path for restoring the source-node head without granting the
  source node publisher authority.

Initial import of the current archive is classified as `legacy_release_import`.
It records the exact archive/tag/commit/checksum/signature and import execution
that actually exist, while naming absent native producer or qualification
evidence. Publisher approval may authorize those imported bytes for bootstrap;
it cannot create historical provenance retroactively.

## Delivery stages

### Stage 1 — immutable publication contracts

- Prove/reuse the canonical portable tree representation.
- Add the minimum typed generation, set, catalog, and evidence contracts.
- Register complete closure edges and bounded validators.
- Provide local package and verification operations.

### Stage 2 — bundle-source node

- Add publisher-authorized catalog publication and exact read/resolve APIs.
- Reuse bounded object transfer and generic signed-head retention.
- Add durable source-node bootstrap, restart, diagnostics, and repair tests.
- Import the current release bundle archive as an initial snapshot.

### Stage 3 — native producer and temporary GitHub adapter

- Express affected-bundle build, assembly, checks, retention, qualification,
  and publication as RyeOS-native operations and a Graph.
- Publish one data-only bundle, then one non-core native bundle.
- Keep a thin GitHub trigger only where current bootstrap requires it.
- Prove bundle-only publication performs no image build and no daemon build.

### Stage 4 — consumer composition

- Fetch and verify exact bundle sets without activation.
- Add full prospective set admission.
- Add deployment-authorized node selections and stopped-node, set-level
  crash-recoverable activation with a defined commit point.
- Start with publisher-curated sets, then admit custom node compositions from
  exact publisher-authorized generations under explicit local policy.
- Prove two consumers can intentionally run different retained generations.

### Stage 5 — stable substrate deployment

- Publish and deploy one general substrate image carrying a small bootstrap seed
  closure. Profile selection activates the bundle-source service where needed;
  the persistent catalog is seeded only when absent.
- Move normal deployment coordinates to image digest plus deployment-authorized
  node-bundle selection, consuming publisher-trust generation, and node-policy
  generation.
- Retain current profile images until update and rollback evidence is green.
- Remove redundant image variants only when their remaining differences are
  selection policy rather than real containment or hardware boundaries.

### Stage 6 — RyeOS-primary release execution

- Make the RyeOS development node the primary release executor.
- Project results back to GitHub rather than treating Actions as authority.
- Add native scheduling, review, and release surfaces only from observed need.
- Connect published set selection to reflexive deployment when that owner is
  pulled forward.

## Acceptance properties

The first production cut is complete only when:

- a deployed source image remains byte-identical across a bundle-only release;
- bundle-only production does not build or publish an OCI image or compile
  `ryeosd`;
- the new generation retains exact scoped evidence claims about source,
  producer, environment, checks, publisher, target, and content, and each claim
  is accepted only under explicit policy;
- source-node restart preserves current and retained catalog generations;
- the bundle-source node cannot forge publisher evidence;
- consumers reject missing, partial, malformed, oversized, wrong-target,
  incompatible, unsigned, or wrongly signed closures;
- one consumer can move to the new set while another remains on the old set;
- interrupted upload, catalog advance, staging, and activation recover without
  inferred success;
- pre-commit recovery and compatible explicit rollback follow their distinct
  contracts and refuse unsupported state reversal; and
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
- A new universal package/build recipe language or parallel content store.
- Automatic self-publication or self-deployment after successful checks.
