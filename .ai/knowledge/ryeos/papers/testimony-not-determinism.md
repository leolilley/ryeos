---
category: ryeos/papers
tags: [papers, verification, testimony, determinism, trust, flagship]
version: "0.2.0"
description: >
  Working notes on attributable testimony, computational reproduction and
  independent evaluation as complementary execution-evidence contracts.
---

# Paper 2 — Testimony, Not Determinism

Working notes, not a draft. Revised September 2026 to distinguish evidence
claims without dividing software into mutually exclusive verification branches.
Assumes the execution-object model; owns the testimony and specification-gap
vocabulary used downstream.

## Thesis

> Reproducing a computation can establish agreement about its output.
> It does not, by itself, establish that the output was a good decision.
> Signed testimony makes claims attributable; independent evaluation tests
> them under an explicit procedure.

RyeOS's opportunity is to connect these forms of evidence to exact executable
capability, admitted authority and retained consequences. Testimony complements
recomputation rather than replacing it.

## The one claim

Open-ended judgment needs an account of who made a claim and under what
authority, alongside whatever task-specific checks can be performed. Retaining
that account separately from the executor makes it inspectable across process
and site boundaries, subject to local trust and evidence availability.

This does not prove that other execution systems cannot host models or people.
It identifies contracts that such systems must also supply if they want the
same evidence and authority properties.

## The two gaps

**Reproducibility gap.** Re-running an invocation may change its output because
of sampling, runtime numerics, batching, hardware or unavailable dependencies.
Recording a seed alone need not close those dependencies. A qualified exact
execution profile can support a narrower reproduction claim.

**Specification gap.** Reproducing a choice is not proving that it meets the
task's intended objective. Some model outputs have exact external checks:
a generated proof, a computed answer, or code evaluated against a stated
contract. Others admit only partial or contested evaluation. The gap belongs
to the claim and task, not to an intrinsic property of all LLM computation.

A deterministic model remains a computable procedure. Reproducing its output
can be meaningful evidence of process conformance without being evidence of
the output's truth or usefulness.

## Authored-output executor

An executor making judgment-bearing choices not fully settled by reproducing
its process. Humans and models can both occupy this role.

This is a working task-relative category, not a sharp division between functions
and minds. A single workflow can combine judgment, exact computation, external
observation and mechanically checkable outputs. No consciousness claim follows.

## What the signature attests

A valid signature establishes that the corresponding key signed particular
bytes, under the verifier's cryptographic and custody assumptions. When those
bytes describe an external event, the event description is attributable
testimony, not cryptographic proof that the world behaved as described.

- Verification checks integrity, signatures and structural contracts.
- Trust policy decides what standing to give the issuer and evidence.
- Authorisation decides which action is permitted now.
- Enforcement constrains what a running process can actually do.
- Evaluation tests task claims under a named procedure.

Trust is not eliminated. Some trust decisions become explicit and inspectable;
hardware, OS, custodians, observation completeness and evaluator quality still
matter. A key may be pseudonymous, shared or compromised. Signatures alone do
not establish a human identity, exclusive history or truthful testimony.

## Capability and consequence

A signed capability without execution history describes what could be invoked.
A log without exact capability and authority bindings leaves ambiguity about
what ran. Connecting both gives a verifier a more useful account:

capability → invocation under authority → recorded consequence.

The links support inspection. They do not establish that every physical cause
or unobserved effect was captured.

## Replay, recomputation and live work

Re-witnessing reads retained evidence. Recorded-effect replay returns an
existing result under its exact reuse contract without contacting the provider
again. Qualified re-execution performs computation again and compares the
claimed outputs within an explicit scope. Live operations follow their own
effect contract.

These operations must remain distinguishable. A missing response is not proof
of non-execution; a new identifier is not a safe way to bypass uncertainty.
Retention loss may make replay unavailable without making a fresh external
action safe.

## Demonstrations to qualify

These are proposed evidence requirements, not a current release certification:

- Verify the exact capability, request and admitted authority for one result.
- Retain an external observation with its issuer and observation limits.
- Replay a recorded effect with independent evidence of zero provider contact.
- Reproduce a qualified computation while keeping that claim distinct from
  semantic evaluation.
- Refuse unauthorised continuation even when historical signatures verify.
- Preserve an explicit uncertain outcome across a dispatch/commit crash cut.

## Objections and limits

- **Can someone sign a lie?** Yes. Attribution supports accountability only
  with retained evidence, meaningful custody, trust decisions and consequences.
- **Does hardware attestation solve it?** It can strengthen claims about the
  executed stack under its threat model. It does not prove an answer useful.
- **Does testimony make reproduction unnecessary?** No. Exact computation and
  independent tests can strengthen or refute a signed claim.
- **Is this exclusive to RyeOS?** No. The hypothesis concerns integrating these
  contracts as common execution infrastructure rather than inventing them
  separately for each application.

## Phrases worth preserving

- Trust is not eliminated; important trust decisions acquire an address.
- Reproducing an answer is not establishing that it is right.
- Capability and consequence belong in the same inspectable account.
- Attributable testimony, not automatic truth.

## Guardrails

Do not equate a key with a person or a signature with complete history.
Do not claim testimony supplies safety, authorisation or semantic correctness.
Do not turn the specification gap into a prohibition on evaluating model
outputs. Current mechanisms require route-specific qualification.
