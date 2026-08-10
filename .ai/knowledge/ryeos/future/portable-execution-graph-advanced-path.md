<!-- ryeos:signed:2026-08-10T03:16:08Z:15bc5a4d81f61da0e90fac00fe220d37c0e723c3ff36d549fbf5f8e9f21e322f:wmtptcO0gpwjYWNQnkAXRn5qSo3kmlWmeEUYZgQ5quMbLkYLrQqtuk3J+jbhnRwpYdKURWO14IZlVXl92TWmAg==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, portable-execution, execution-graph, architecture, export]
version: "0.3.0"
status: deferred
description: >
  Remaining deferred scope for portable execution: capsule/evidence export and
  attestation. The identity and projection layers this note originally
  deferred were implemented in 2026-08.
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
  `bundles/standard/.ai/knowledge/ryeos/core/engine/effective-programs.md`.

What remains deferred is only the word **portable**: a chain leaving its node.

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
4. **Continuation across nodes** — explicitly out of scope here; owned by
   `distributed-substrate-deferred-advanced.md` (closure transfer, hosted
   isolation handoff) and gated on `key-lifecycle.md` (a traveling capsule
   makes signer succession the importer's problem too).

## Guardrails (carried forward)

- No export API until the format/profile above are contracts.
- No trust or authorization semantics derived from hashes alone.
- Verification-only import first; continuation is a separate, later contract.

## Triggers to revisit

- a chain needs to be shown to anyone who does not trust the node (audit,
  benchmark submission, publication of an ARC solve);
- an ARC campaign report needs to become an independently verifiable solve
  proof rather than a node-local projection;
- distributed-substrate pull-forward work starts (closure transfer wants this
  format);
- key-lifecycle succession design lands (attestation depends on it).
