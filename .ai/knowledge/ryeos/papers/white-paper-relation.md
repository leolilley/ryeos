---
category: ryeos/papers
tags: [papers, white-paper, scope, contract]
version: "0.1.0"
description: >
  The contract between the four-paper series and the white paper thesis:
  scope boundaries, the v0.3.0 insertions applied when the thesis moved
  into this folder, and the inherited guardrails with one recorded
  divergence.
---

# Relation to the white paper

This file is the contract between the paper series and
`portable-execution-white-paper-thesis.md`. Its job is to keep the two
from drifting into each other: editing the series must never silently
change the white paper's scope, and vice versa.

## The standing terms

- The white paper keeps its own thesis — **portable verified execution
  through cryptographic identity** — as a general systems claim, with
  agents demoted to one application among several. It is independently
  publishable and is **not** "paper zero."
- The series does not restate the white paper's thesis; papers cite it as
  the positioning ancestor. The `capability and consequence` refinement is
  owned by the white paper; paper 2 references it as the mechanism of
  testimony.
- The white paper leads with the fused property, not with "everything is
  data" (its own guardrail: mechanism, not headline). The series can
  afford the ontology paper (`execution-is-an-object.md`) because papers
  argue theses, not products.

## History

The thesis document lived at
`.ai/knowledge/ryeos/future/portable-execution-white-paper-thesis.md`
(v0.2.0, signed). On 2026-07-23 it moved into this folder and was updated
to v0.3.0; the move and edits invalidated its signature, which was
stripped pending re-sign.

## The v0.3.0 insertions

Three additions from the series work, applied directly to the thesis doc:

1. **The necessity argument** (new section after "Important refinement").
   The doc previously *asserted* that portable and verified are one fused
   property; the insertion *derives* it — unbind execution from location
   and the machine can no longer vouch, so the data must vouch for itself.
   Portability forces proof. An asserted fusion is a design choice; a
   forced fusion is a theorem.
2. **The branch-point paragraph** (in "Adjacent systems and distinction").
   Sharper than the layer distinction: the verified-execution lineage
   verifies by recomputation and therefore requires spec-carrying
   executors; RyeOS verifies by signed testimony. The adjacent systems are
   mostly on the other branch of verification theory, not merely at a
   different layer. Points to `testimony-not-determinism.md` for the full
   argument.
3. **Operators-founded-the-class** (in section 4, after the actor-primitive
   paragraph). The actor class always contained authored-output executors —
   human operators — so the substrate's juridical shape predates any model,
   and models slot in without a new primitive. This satisfies the
   "don't center agents" guardrail while keeping the deep claim.

## Inherited guardrails

The series adopts the white paper's guardrails wholesale: identity does not
mean safety; do not overstate deterministic replay; distinguish identity,
trust, authorization, isolation, and execution; do not reduce RyeOS to a
registry or package manager.

## The one recorded divergence

The white paper's guardrail "do not center the paper on AI agents" is a
scoping decision for *that document*, not a ban on the question. The series
deliberately gives agents a whole paper (`the-corporate-agent.md`) —
legitimately, because the series has upstream results (papers 1-2) for the
agent paper to inherit from, which the white paper, as a standalone
document, does not. The agent paper adds no primitives; that is the
condition under which the divergence stays honest.
