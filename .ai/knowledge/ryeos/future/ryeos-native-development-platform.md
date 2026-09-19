<!-- ryeos:signed:2026-09-18T23:27:55Z:f17f67a3f132779d8f6dc24cc8b9c596158f79b24e2b9702f853d2e653689bff:YTdx2qimkspIpIe6/eBtdeqBycIzjXLDCVnji/HmSjtlSqOurP66ycOpDsxTyTIW3A+aXI6lM7JJu+tqPXasAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: ryeos-native-development-platform
title: RyeOS-Native Development Platform
description: Long-term owner for RyeOS-native project hosting, checks, review, release execution, and GitHub projection
entry_type: design
version: "1.2.0"
status: deferred_end_state
```

# RyeOS-native development platform

## Status

This document owns the deferred end state. It is not a claim that RyeOS-native
source hosting, release execution, or deployment has landed. The narrower
native bundle-publication slice is scheduled separately in
[Native bundle publication and node composition](native-bundle-publication-and-node-composition.md).

## Direction

The current development loop still uses GitHub-era assumptions:

- source coordination through Git refs and pull requests;
- broad validation through local test scripts or GitHub Actions;
- bundle publication coupled to checked-in bundle trees and image production;
- expensive build and test artifacts landing on a developer or CI machine;
- review discussion, checks, artifacts, releases, and source changes split
  across tools; and
- external workflow YAML carrying orchestration that RyeOS cannot inspect as
  one admitted execution.

Long term, a RyeOS development node should be able to host an exact project
generation, coordinate changes, execute build and qualification graphs, retain
products and evidence, authorize releases through a separate publisher role,
and drive deployment. GitHub remains useful as an interoperable source mirror,
trigger, review projection, release mirror, or bootstrap channel. It is not the
intended owner of release semantics.

GitHub concepts become projections of richer RyeOS-native objects:

```text
RyeOS project and development nodes
  source generations, candidates, checks, reviews,
  retained products, qualifications, releases
                         |
                         | mirror / import / trigger / status projection
                         v
GitHub compatibility
  commits, pull requests, Actions, releases, statuses
```

Moving execution from a GitHub-hosted runner to a self-hosted shell runner is
not this end state. The source, program, environment, authority, products, and
evidence must be admitted and retained through RyeOS contracts.

## Native software lifecycle

The complete direction is:

```text
exact source generation
  -> bounded implementation campaign or authored change
  -> frozen candidate
  -> admitted build and check Graphs
  -> retained products and qualification evidence
  -> explicit publisher authorization
  -> immutable bundle/environment/substrate publication
  -> exact consumer or deployment selection
  -> separately authorized activation Graph
```

The stages may initially run on one physical node, but they remain distinct
authority roles:

- a project or source owner selects the source generation;
- a development/build node executes admitted operations;
- qualification records what was actually checked;
- a publisher authorizes an exact product or release coordinate;
- a bundle-source node stores and serves immutable published closures;
- a consumer applies its own trust and admission policy; and
- deployment authority selects and activates an exact admitted release.

Build success is not publication. Publication is not installation. Installation
is not activation. A candidate, worker, development node, or bundle-source node does
not acquire the next authority merely because it completed the prior stage.

## Build and release composition

Release production should reuse the existing RyeOS substrate:

- signed Tools own bounded build, test, packaging, and verification operations;
- Graphs own ordering, fan-out, failure handling, and durable lineage;
- exact source closures and environment bindings identify the inputs;
- retained products and content manifests identify outputs;
- attestations state scoped claims about those outputs;
- a typed bundle-publication section of operator-signed node policy determines
  which publisher and qualified product may become a release;
- signed heads and CAS closures retain and distribute published generations;
  and
- consumer admission and node policy remain local to the consuming node.

Release logic belongs in these operations and contracts. External workflow
files may invoke them, but must not become the only implementation of bundle
ownership, compatibility, qualification, signing, or release transitions.

The environment producer in
[RyeOS environment build system](environment-build-system.md) supplies exact
build and worker environments. It does not own project releases. The bundle
publication design consumes those environments and retained products without
turning environment production into a package manager or release authority.

## GitHub transition

The migration is deliberately incremental:

1. Keep Git and the current GitHub release path operational.
2. Move reusable build, check, package, and publication behavior behind
   RyeOS-native Tools, services, and libraries.
3. Make GitHub Actions a thin adapter that selects an exact source coordinate,
   invokes the RyeOS operation, and projects its result.
4. Prove that a GitHub-triggered and a RyeOS-triggered execution over the same
   admitted inputs select the same product identity.
5. Make the RyeOS development node the primary executor while GitHub remains a
   trigger and mirror.
6. Move source coordination or review hosting only after source generations,
   check records, artifact isolation, recovery, and review surfaces are proven.

At every stage, external CI may remain an honest bootstrap exception. The
exception must be named rather than hidden inside a supposedly native Tool.

## Relation to self-hosting and deployment

[Self-hosted implementation campaigns](self-hosted-implementation-campaigns.md)
own bounded work that proposes a frozen candidate. The worker cannot exercise
candidate disposition; an admitted operator may separately publish the accepted
candidate to the project HEAD. Project-HEAD publication is not bundle release
publication.

[Native bundle publication and node composition](native-bundle-publication-and-node-composition.md)
owns the first concrete release/distribution slice: independent bundle
generations, exact bundle sets, bundle-source serving, and consumer composition.

[Reflexive deployment](reflexive-deployment.md) owns the later activation Graph
that changes a running node or epoch. It consumes an already-published exact
release coordinate and does not reinterpret build evidence as deployment
authority.

## Near-term implementation boundary

The first useful slice is not a general GitHub replacement. It is native bundle
publication:

1. Build an exact clean candidate from a retained or locally admitted result.
2. Use a constrained publisher transaction to sign the final in-tree bundle
   manifest/items, then capture and qualify that exact signed tree.
3. Finalize and release-attest the bundle generation through the separate
   publisher boundary.
4. Publish it to a bundle-source node and construct an exact curated bundle set.
5. Let another node fetch, verify, stage, and prospectively admit that set;
   deployment authority then authorizes an exact node-bundle selection for
   activation through the operator-signed whole-init fence.
6. Invoke the same operations from GitHub only as a temporary adapter.

This slice provides immediate release-cost reduction while exercising the same
source, Graph, product, evidence, publication, and consumption boundaries the
full development platform needs.

## Non-goals

- Do not replace Git interoperability merely to claim self-hosting.
- Do not block current GitHub-based releases before a qualified replacement
  exists.
- Do not move source hosting before project-generation and recovery contracts
  are reliable.
- Do not give implementation workers or general build nodes publisher keys.
- Do not treat a mutable working tree, CI workspace, or terminal transcript as
  release truth.
- Do not add a universal pipeline language beside existing Tools and Graphs.
- Do not combine build, publication, catalog serving, and deployment into one
  ambiently privileged node role.
- Do not make RyeOS self-hosting synonymous with automatic self-modification.

## Acceptance properties

The end state is credible when:

- the exact source, operation Graph, environment, result, checks, and publisher
  decision are connected by durable evidence;
- GitHub can disappear for one release execution without changing release
  semantics;
- GitHub projection can be restored without becoming the authoritative record;
- a build node cannot publish or deploy merely because its checks passed;
- a bundle-source node cannot forge publisher-authorized releases;
- consumers select exact releases and apply independent local admission;
- interrupted build, publication, and deployment stages recover without
  inferring success; and
- RyeOS can build and release a successor while the installed generation
  remains the authority evaluating that successor.
