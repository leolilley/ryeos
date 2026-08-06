<!-- ryeos:signed:2026-08-06T03:37:10Z:ad7d693266df6967391f0b2756b30b82c582ea18d84bc5b1837e99b7a5122e69:JsT51sSnWc3iDqBBi5TkmYhTGTWqucOl7SOoD1n7TrkQTUkagoARlqSLn/cQTNxZyw1rNQAeDhAteM1QogdkAw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, signing, trust, keys, revocation, recovery]
version: "0.1.0"
status: deferred
description: >
  Signer rotation, succession, and delegation for a substrate whose sealed
  history is gated by revocation checks.
---

# Key lifecycle

Everything now hangs off signatures: bundle content under the publisher key,
runtime-authored content under daemon keys, configured hook sources carrying
their own bounded grants, and — since the effective-program cut — **recovery
itself gated by current revocation policy for every contributor signer,
including hook-plan sources**. Revocation became retroactive governance: the
past stays immutable as evidence, but its authority to continue is re-litigated
in the present.

That last property is why lifecycle needs design *before* it needs
implementation: every month of sealed history accumulated under today's keys
raises the cost of getting succession wrong later.

## What is currently true

- A small, controlled key universe: one dev publisher fingerprint, the
  operator key, daemon runtime keys, per-space trust classes
  (bundle/node/project) pinned in the hook source-ownership table.
- Revocation blocks recovery (tested:
  `captured_hook_source_signer_revocation_blocks_recovery`).
- No rotation, succession, or delegation story exists anywhere.

## The questions a design must answer

1. **Succession without orphaning.** If the publisher key rotates, every
   sealed capsule's contributors still carry the old fingerprint. Recovery
   revalidates against *current* policy — so succession needs an attestation
   path ("key B succeeds key A as of T, signed by A") that lets old-signed
   history remain recoverable without keeping A live. Without this, rotation
   is indistinguishable from revoking your own past.
2. **Revocation granularity.** Today revocation is per-signer. A compromised
   key should invalidate *future* trust without automatically poisoning every
   *historical* chain it touched — or should it? The current fail-closed
   answer (blocks recovery) is defensible; it must become a decision, not a
   default.
3. **Delegation.** Hosted-node and multi-project futures
   (`hosted-node-trust-boundaries.md`, `public-first-contact-live-node.md`)
   imply scoped authorities (sign only project content; sign only hook
   config). The grant machinery from the effective-program work is the
   template: bounded, source-owned, declared.
4. **Key events as evidence.** Rotation/revocation/succession should be
   durable admitted events (same posture as admission evidence) — the trust
   timeline is part of the account of the past.

## Non-goals

No PKI product, no external CA, no federation trust (that belongs to
`distributed-substrate-deferred-advanced.md`). This is single-operator key
hygiene done before it is urgent.

## Triggers to revisit

- first genuine need to rotate any key (schedule the design *before* this);
- hosted-node or first-contact work starts;
- capsule export/portability work starts (a traveling capsule makes the
  succession question another node's problem too).
