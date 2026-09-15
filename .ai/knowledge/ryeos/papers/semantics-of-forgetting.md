---
category: ryeos/papers
tags: [papers, forgetting, compaction, privacy, permanence, open-theory]
version: "0.2.0"
description: >
  Working questions about retention changes, preserved verification claims,
  deletion limits and accountability in a content-addressed execution history.
---

# Paper 4 — A Semantics of Forgetting

Working notes, not a draft. The open-theory paper. Revised September 2026 to
remove the assumption that hashes alone preserve every verification predicate
or that cryptographic history requires permanent retention of all commitments.

## Thesis

> A system that derives authority from retained history needs an explicit
> account of what a retention change preserves, what it makes unverifiable,
> and which operations must subsequently refuse.

Storage is finite and records can be sensitive. The goal is neither permanent
surveillance nor silent loss of justification. It is honest retention under
declared verification, privacy and continuation requirements.

## The research question

For a specified surviving closure and set of supported queries, when does
deletion or compaction preserve the promised verification and attribution
claims? Which claims must instead become unavailable or weaker?

"Meaning-preserving" is relative to those named claims, not every conceivable
future query or interpretation. Establishing a useful formal contract remains
research work.

## Content and commitment

A retained hash can identify bytes if they become available again. By itself,
it cannot reveal their contents, validate their schema, establish a grant's
scope or reproduce a result.

Dropping a body while keeping its signed commitment can preserve limited
attribution. It may destroy the evidence needed for replay, continuation or
substantive evaluation. Those losses must be recorded and respected.

There is no universal skeleton of hashes and signatures sufficient for every
authority predicate. Required policy bodies, custody evidence and dependency
closures vary by operation.

## Forgetting, correction and repudiation

Correction publishes a new judgment or current selection without rewriting
what retained earlier records say. Withdrawal from future use is different
from deleting source bytes, revoking authority or denying a prior commitment.

A signed retained record resists undetected alteration relative to a trusted
head. It does not ensure that someone retains that head forever, disclose all
alternative branches, or prevent a signer from denying its significance.

Retention policy can authorise deletion. The system must not continue claiming
evidence completeness after that evidence is gone. Privacy can legitimately
require giving up some verification or continuation capability.

## Encryption and key destruction

Destroying the only remaining decryption material can make retained ciphertext
unreadable under the encryption assumptions. This requires accounting for
backups, wrapped keys, exported keys, plaintext copies and derived artifacts.

Key destruction does not remove an already disclosed copy or erase knowledge
from a person or model. Ciphertext length, metadata and low-entropy commitments
may still disclose information. Encryption-key rotation is not automatically
crypto-shredding.

## Reclamation and compaction

GC computes reachability under selected roots. Selecting the correct roots is
an authority and lifecycle decision: active executions, staged imports,
recoverable handoffs, retained evidence and publication may each impose roots.

Compaction needs a declared equivalence condition. For the supported surviving
queries, identify which answers must remain unchanged and which evidence is
intentionally no longer available. Rewriting a signed object creates a new
identity; any claimed relation to its predecessor needs appropriate evidence.

A signed assertion that compaction was safe is testimony about the operation,
not by itself a proof of query equivalence.

## Demonstrations and open obligations

These are qualification targets, not assertions that a universal forgetting
mechanism has landed:

- Remove unreachable synthetic content without breaking retained closures.
- Refuse replay or continuation when a required retained dependency is absent.
- Distinguish content deletion, current withdrawal and authority revocation.
- Trace a synthetic private source through summaries, indexes, export and
  adaptive artifacts; identify copies the node cannot recall.
- State and test one concrete compaction-equivalence contract.
- Verify the actual key and copy assumptions of a bounded encryption-deletion
  experiment rather than claiming global erasure.

## Relation to the programme

Durable accountability has costs. Papers about portable work and enduring
mandates must state retention requirements rather than treating permanence as
free. Measurement needs evidence coverage; knowledge correction needs known
consumers; neither justifies recording every personal activity indefinitely.

## Guardrails

No universal permanent-accountability floor is established here.
Do not equate a commitment with its missing evidence or deletion with unlearning.
State limitations without claiming legal erasure compliance.
Changes to retention mechanisms require their existing owners and tests; this
paper defines questions, not a new GC implementation.
