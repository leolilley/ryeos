<!-- ryeos:signed:2026-07-23T10:04:39Z:b469e7902fe1b588a7db4c17613474dca1c595384ee1aadc7b70122b42fedb72:16TCbgOawutyQXwcMpg1qFF5v5oOqgUSjklF+/lvRqL+73ErofCoZmYlM4AFustpToTeUe2Bf8nh0ruTQkwoCg==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
---
category: ryeos/papers
tags: [papers, verification, testimony, determinism, trust, flagship]
version: "0.1.0"
description: >
  Working notes for paper 2, "Testimony, Not Determinism" — the flagship.
  Verification by recomputation requires an external spec; executors that
  are their own spec can only be attributed and held to account. RyeOS is
  the constructive existence proof of the testimony branch.
---

# Paper 2 — Testimony, Not Determinism

Working notes, not a draft. The flagship of the series. Assumes paper 1's
ontology (the run as durable object); owns strain points 1 and 2 from
`series-map.md`.

## Thesis

Short version:

> A substrate that verifies by recomputation can only host functions; a
> substrate that verifies by testimony can host minds.

("Minds" is rhetorical shorthand and must be cashed out — see the executor
class below. The precise form:)

> Recomputation verifies executors that have specifications; testimony
> governs executors that are their own specification. LLMs are the first
> non-human executors in the second class, which is why fifty years of
> verified execution produced nothing that could host them.

## The one claim

Verification-by-recomputation requires an external correctness predicate
for the executor. For executors that have none, recompute-verification is
not hard — it is *undefined* — and the best achievable epistemic relation
is attribution: signed testimony over a durable record, judged by local
trust policy. RyeOS is the constructive proof that this branch exists and
suffices.

## The two gaps

The argument must separate a shallow gap from a deep one, or it collapses
into an engineering complaint.

**The reproducibility gap (shallow, contingent).** LLM inference is not
bit-reproducible in practice: sampling, batching effects, floating-point
nondeterminism, provider-side weight updates. But reproducible randomness
is solved — record the seed, replay the draw. A coin flip stays on the
function side of the line, and a lab could in principle sell deterministic
inference. If this were the whole argument, testimony would be a workaround
for a fixable gap.

**The specification gap (deep, essential).** Recompute-verification
requires a definition of correct that exists independently of the
executor. A compiler, a build, a smart contract have this: the function is
the spec, the executor is interchangeable, and a mismatch on re-run *tells
you something*. An LLM has no spec independent of itself; its behavior is
the only definition of its behavior. Even under perfectly deterministic
inference, replay proves only that the model said what the model said — it
can never prove the output was right, because "right" is not defined
anywhere outside the executor. The output is not determined by a spec; it
is **authored** by a judgment. No amount of engineering crosses this gap,
because the gap is not about bits — it is the absence of a correctness
predicate.

## The executor class (definition owned here)

**Authored-output executor**: an executor whose acts are authored rather
than specified — whose interesting outputs are judgment calls among many
admissible continuations, with no external fact-of-the-matter to check
against. It is its own spec.

- The boundary is sharp and consciousness-free: the moment an executor's
  output stops having an external correctness predicate, you have crossed
  from physics into jurisprudence.
- **Human operators are the founding members.** We never verify a human
  decision by re-running the human; civilization built signatures, records,
  contracts, and standing *because* accountability is what remains when
  there is no spec.
- Models are the second member of the class. This is why RyeOS treats
  operator, node, and agent uniformly as signing keys — not ecumenical
  politeness but the structural tell that the substrate's verification
  model was testimony-shaped before any model arrived.

## The branch point

The verified-execution lineage — Nix, reproducible builds, deterministic
replay, blockchains, durable-execution engines — all verifies by
recomputation, and therefore structurally requires spec-carrying executors.
This is why none of it ever produced an AI substrate, and why none of it
can be extended into one: the limitation is not layer or maturity but
branch. RyeOS is not those systems applied at a different layer; it is the
other branch of verification theory — attribution over a self-certifying
record — which had to exist before execution-by-judgment could be a
portable, verifiable object at all.

## What the signature actually attests (strain 1)

The world is not content-addressed; a tool touches filesystems, networks,
providers. So the signature at the frontier attests not "this happened" but
"this key stands behind the claim that this happened." Consequences:

- RyeOS is an **attribution system**, not a verification system. It never
  promises truth; it promises that every claim has a claimant, and that no
  one lies anonymously or retracts silently.
- **Trust is localized, never eliminated.** Every ambient trust assumption
  (this machine, this PATH, this perimeter) becomes a discrete,
  inspectable, revocable decision about a key. Trust is given an address.
- The attestation split already in the white paper is the exact mechanism:
  verification is objective; trust is a local decision applied to the same
  evidence everywhere — which is what lets evidence move between parties
  at all.
- The system is juridical, not mechanical: a court record, not a physics
  engine. Its guarantees are the guarantees of testimony under signature.

## Replay on this branch (strain 2)

Replay means **re-witness, not recompute**: reading the record back, never
re-running the world. Nondeterminism exists only at the frontier; completed
steps are consumed as data and never re-crossed (paper 1). This is not a
weaker replay — it is the only coherent replay for a substrate whose
executors include authored-output executors.

## Capability and consequence as the mechanism of testimony

The white paper's strongest refinement — "a signed tool without history is
a package; a history without signed capability is a log" — is this paper's
mechanism. An accountable act is precisely a warrant joined to a record by
one identity: what was permitted, linked by hash and key to what was done.
Testimony without the capability side is unauthorized claim; without the
history side it is unexercised power. The two strongest ideas in the
program are one idea viewed from two sides.

## Evidence in the implementation

- Signing as the universal act; keys as the only actors, uniform across
  operator, node, agent.
- Attestation objects: issuer key, subject hash, claim, policy, evidence —
  objective verification, local trust.
- Trust fold-back (weakest-link inheritance) and source-space trust caps —
  trust policy as computation over evidence.
- The capability wire format and effective_caps propagation surviving
  restart via ResumeContext — the warrant side of the warrant/record join.
- Events replay, thread tail/chain — the re-witness operation, implemented.

## Objections and current answers

- **"Testimony is weaker than verification."** Category error: it is not a
  weaker version of the same relation but the correct relation for a class
  where recompute-verification is undefined. Also, recompute systems do not
  escape trust — they relocate it into specs and oracles.
- **"Trusted hardware / attestation solves this."** TEEs attest that
  specific code ran. That reduces the reproducibility gap, not the
  specification gap: attested inference still has no correctness predicate.
  Complementary, not competing.
- **"So anyone can sign lies."** Yes — non-anonymously, unretractably,
  under a key whose trust is a revocable local decision. That is the same
  deal human institutions run on, and it is the honest maximum; no
  substrate can content-address reality.

## Phrases worth preserving

- Recomputation verifies executors that have specs; testimony governs
  executors that are their own spec.
- The gap is not about bits; it is the absence of a correctness predicate.
- Trust is never eliminated; it is given an address.
- No one lies anonymously, and no one retracts.
- A court record, not a physics engine.
- Attested inference still has no correctness predicate.

## Guardrails

- Always cash out "minds" as the executor class; never let the shorthand
  carry the argument. No consciousness claims.
- Do not frame as agent-specific: operators found the class. Agents appear
  only as the second member (and get their own paper downstream).
- Do not overstate: testimony does not provide safety, semantic
  correctness, or authorization — inherited from the white paper's
  identity/trust/policy separation.
- Be honest about design history where relevant: the substrate may have
  landed on the testimony branch partly by construction and partly by
  discovery. The necessity argument stands either way.
