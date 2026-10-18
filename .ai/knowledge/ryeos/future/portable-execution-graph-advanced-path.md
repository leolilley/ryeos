<!-- ryeos:signed:2026-10-18T01:26:29Z:1fe548888603ea752c095a268592ccb861b707518b5e7376d972bb71942c41e2:/8hH8uv/t0nte8j6R0OhCFMLM9Ap2rCMK851v8jd5G0KgbDCC6tjv9B79TuoPYlFHTi2dtsZUMvx+rguEcH1DQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [future, portable-execution, execution-graph, architecture, export]
version: "0.4.0"
status: scheduled
description: >
  Remaining scope for portable execution: independently complete capsule and
  evidence export plus attestation. Hosted-worker checkpoint transfer and
  explicit cross-site continuation are already implemented.
---

# Portable execution: deferred export/attestation path

Rewrite, 2026-08. The 0.1 version of this note deferred a four-layer model
(portable capability, invocation instance, realized consequence, projection)
until "stable identity breadcrumbs" existed, and described a
`definition_hash` identity bridge. Both halves are now history:

- The four layers were implemented by the execution-field, hook-evidence, and
  effective-program packages: **capability** = signed items + effective
  definitions (`effective_definition_digest`, families by canonical ref);
  **invocation** = admitted launch capsules sealing the complete resolution;
  **consequence** = durable events, receipts, hook observations, state
  anchors, artifacts under chain identity; **projection** = the field's
  facts/VM contracts (`ryeos.ui.field.facts.v2`).
- `definition_hash` no longer exists. Executable identity is
  `effective_definition_digest`; `root_raw_content_digest` is source
  evidence; `admitted_launch_capsule_hash` is invocation authority. See
  `bundles/ryeos-docs/.ai/knowledge/ryeos/core/engine/effective-programs.md`.

Portable hosted-worker environments, checkpoints, staged transfer, remote
admission, and cross-site continuation now exist for one explicitly selected
placement. What remains here is a different claim: an independently complete,
disclosure-scoped export that a verifier can authenticate without continuation
authority. Its place beside hosted placement and before broader federation is
summarized by `knowledge:ryeos/future/substrate-growth-roadmap`.

## Remaining scope

A capsule plus its evidence chain is internally hash-checkable under its
retained signer evidence. It is not yet an independently complete/authentic
export: that claim requires the content-complete closure and signed export-head
attestation below. The missing pieces are transport-shaped, not
identity-shaped:

1. **Export closure format.** One archive: capsule, CAS closure (definition
   contributors, hook-plan sources, manifests, artifacts, dispatch/provider
   effect records, first observations, accounting/publication proofs, and
   execution realizations), plus the chain's event history with signatures —
   content-complete for independent
   verification, with an explicit statement of what is *excluded* (secrets,
   vault material, host paths — the sealed request's sanitization boundary is
   the template).
2. **Verification profile.** What a second party checks and in what order:
   capsule hash → contributor signatures → effective-digest recomputation →
   realization/attestation closure → event-chain integrity → effect-coordinate
   and publication-proof conformance. Effectively the recovery path minus the
   authority to continue; specify it as a read-only profile.
3. **Attestation statement.** A signed claim by the exporting node binding
   the export to its head state ("this is the complete history of chain X
   through seq N as of T"), so partial or pruned exports are detectable.
4. **Continuation relationship.** The landed hosted-worker handoff already
   transports the exact private environment/checkpoint and preserves one chain
   root across placement threads. That operational transfer is not an
   independently publishable execution proof. Broader graph continuation,
   third-party import, and federation policy remain owned by
   `distributed-substrate-deferred-advanced.md` and `key-lifecycle.md`.

The preferred first consumer is a completed execution proof exported for
independent verification. That keeps the first portable slice read-only and
gives closure completeness, disclosure, and attestation claims a concrete
acceptance case before remote continuation is authorized.

## Guardrails (carried forward)

- No public or certification export API until the format/profile above are
  contracts.
- No trust or authorization semantics derived from hashes alone.
- Verification-only import grants no continuation authority. Existing hosted-
  worker continuation remains governed by its placement-transfer contract.

## Triggers to revisit

- a chain needs to be shown to anyone who does not trust the node (audit,
  benchmark submission, publication of an execution result);
- a project report needs to become an independently verifiable execution
  proof rather than a node-local projection;
- distributed-substrate pull-forward work starts (closure transfer wants this
  format);
- key-lifecycle succession design lands (attestation depends on it).
