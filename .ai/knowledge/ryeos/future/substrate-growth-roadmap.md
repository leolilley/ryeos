<!-- ryeos:signed:2026-08-27T04:21:34Z:96e730d98c7a7fa7db70cdfc018d0f6fd84d0989324a135b76c2b325001e03f5:/xmQRgaSrrNcxTYDDGM+XR4HAt6c4fdOB6hAL2ig7Nmhguw2eMk/YvY/8D5542CW961pYnIegJA65xSP4ixtCw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/future
name: substrate-growth-roadmap
title: RyeOS Substrate Growth Roadmap
description: Boundary map from landed exact execution and portable hosted-worker placement through local inference, self-hosted implementation, broader federation, and deployment
entry_type: reference
version: "0.2.0"
```

# RyeOS substrate growth roadmap

This note connects the current execution substrate to the next RyeOS
directions. It is a sequencing and ownership map, not an implementation plan.
The linked owner documents remain authoritative for each boundary.

## Landed horizon

RyeOS no longer stops at single-node execution. The common substrate now
includes:

- signed programs, exact project/content authority, admitted launch capsules,
  durable consequences, restart recovery, and rebuildable projections;
- immutable recorded effects with first-bank versus replay evidence;
- deep continuation and bounded follow cohorts under pinned project authority;
- hosted structured worker executions with private homes, credential
  generations, typed commands/effects/approvals, candidate publication, and
  root-chain authority;
- portable worker environments and checkpoints;
- a stable `chain_root_id` for the execution lineage, distinct current
  `placement_thread_id`, and boot-epoch fencing;
- staged placement-transfer authority, remote admission, and durable cross-site
  worker handoff; and
- verification and recovery checks that preserve source/target chain-writer
  authority rather than discovering state sideways.

This is not yet a general scheduler, public federation fabric, or hostile
multi-tenant cloud. It is the narrower portable hosted-execution path required
to place one admitted worker on an explicitly selected site and retain one
authoritative chain.

## Growth map

```text
exact single-node execution + recorded evidence
    |
    +-- hosted worker execution (landed)
    |     `-- portable checkpoint + explicit cross-site placement (landed)
    |
    +-- local inference (active next branch)
    |     |-- serious recorded remote tinygrad profile
    |     |-- traces + corpus + recorded training
    |     |-- sealed qualification, when proved
    |     `-- generation-state capsules, when measured
    |
    +-- self-hosted implementation campaigns
    |     `-- frozen candidate + checks + explicit publication
    |
    +-- verification/certification export
    |
    `-- broader federation and hosted multitenancy
          `-- scheduling, delegation, repair, distributed retention
```

The branches compose through the same identity, content, consequence, and
lineage contracts. They do not justify parallel model, worker, checkpoint, or
federation substrates.

## Active local-inference branch

The offline-model requirement and a capable remote site now satisfy the pull-
forward gate. The next path is not “seal the CPU fixture” and not “build a
general scheduler.” It is:

1. prove ordinary disabled-isolation local inference through the existing
   daemon-owned private workspace;
2. compose the reusable tinygrad worker with one explicit signed serious
   model/runtime/target profile;
3. place it on the selected capable site and bank/replay a real request;
4. retain bounded performance, trace, and artifact evidence;
5. admit provenance-complete corpora and recorded tinygrad training when that
   evidence exposes a useful learning loop;
6. qualify a sealed scope only after closed observed-artifact promotion and a
   two-process byte proof; and
7. export the selected execution closure for network-independent acceptance.

Recorded generation capsules may support honest prefix/resume/fork before
sealed qualification. Exact equivalence remains a separately earned claim.
Owner: `knowledge:ryeos/future/local-execution-roadmap`.

## Self-hosted implementation branch

The hosted-worker substrate can now be qualified as an implementation vessel.
The first campaign shape is one owner-issued, bounded, disconnect-safe turn in
a private RyeOS candidate workspace. The installed host generation remains the
authority; the candidate can edit, build, test, and return evidence but cannot
sign, install, restart, or publish itself.

Local inference expands that campaign after the recorded serious route exists:
cheap retrieval, review, test generation, failure clustering, and repeated
hypothesis evaluation can run as admitted child executions. Frontier hosted
coding remains useful during bootstrap and is not part of the final offline
execution closure.

Owner: `knowledge:ryeos/future/self-hosted-implementation-campaigns`.

## Portability and evidence branch

Cross-site worker placement has landed for one admitted lineage. Portable
evidence remains a distinct problem: an external verifier should receive the
exact capsule and content/effect/evidence closure plus a deterministic
verification profile and signed chain-head statement. Verification does not
grant continuation authority.

Certification adds named retention and disclosure policy; it is not “keep
every cache forever.” Offline model acceptance is the first concrete consumer
of a content-complete verification closure.

Owner: `knowledge:ryeos/future/portable-execution-graph-advanced-path` and
`knowledge:ryeos/future/provider-call-effect-records`.

## Broader hosted and federation branch

The landed placement path deliberately precedes broad federation. Later work
still includes:

- multi-principal project, cache, credential, quota, audit, and cleanup
  boundaries;
- threat-model-selected outer containment for unrelated or hostile tenants;
- node inventory and policy publication suitable for selection;
- generic placement policy and scheduler leases beyond explicit target choice;
- key succession, revocation, delegation, audience binding, and replay
  protection;
- distributed retention, repair, mirrored heads, and availability policy; and
- public or third-party admission boundaries.

Federation transports and re-admits existing objects and consequences. It must
not replace `chain_root_id`, project authority, effect identity, placement
threads, or launch capsules with a looser distributed identity.

Owners: `knowledge:ryeos/future/distributed-substrate-deferred-advanced`,
`knowledge:ryeos/future/hosted-node-trust-boundaries`, and
`knowledge:ryeos/future/key-lifecycle`.

## Pull-forward order

1. Qualify the landed hosted-worker and explicit cross-site placement path.
2. Activate one serious recorded tinygrad model on the selected remote site.
3. Use a bounded hosted implementation campaign to complete and qualify the
   missing RyeOS local-inference path, with operator-controlled publication.
4. Shift useful project work to the admitted local route, retaining traces and
   comparisons.
5. Add corpus building and recorded training when a concrete local learning
   opportunity is measured.
6. Complete offline verification/export for the selected final execution.
7. Pull sealed qualification and generation-state exactness only where their
   stronger claims provide measured value.
8. Add general scheduling, hostile multitenancy, and broader federation only
   after explicit placement becomes the limiting operation.

Reflexive deployment remains separate throughout. A successful implementation
worker or candidate-node probe cannot activate its own candidate.

## Decision rule

| Observed need | Pull forward |
|---|---|
| Capable remote device is known and selected | explicit serious local-inference profile |
| Repeated work exposes useful trajectory/outcome data | admitted corpus and recorded tinygrad training |
| Same-scope clean processes reproduce canonical bytes | sealed local qualification |
| Prefill/recovery/search cost is material | recorded generation capsules; exact semantics after proof |
| One hosted turn cannot meet mechanical acceptance | bounded evidence-gated campaign controller |
| A completed run must be audited elsewhere | verification/certification export |
| Explicit target choice becomes operationally limiting | inventory-driven placement and scheduler policy |
| Unrelated principals share infrastructure | hosted tenant boundary and stronger containment |
| Authority/content must move among independent sites | broader federation, key lifecycle, repair, retention |

Landing a mechanism is not evidence that its broadest consumer should be built
next.
