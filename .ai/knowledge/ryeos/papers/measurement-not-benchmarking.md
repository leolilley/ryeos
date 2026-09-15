---
category: ryeos/papers
tags: [papers, measurement, intelligence, observers, frames, research-note]
version: "0.2.0"
description: >
  Standalone research note beside the four-paper program: measurement of
  authored-output executors is testimony — a fold under a named observer
  frame over a signed record. Owns the frame/fold/invariant vocabulary and
  the open quotient conjecture. Candidate paper 5 only by future decision
  recorded in series-map.md.
---

# Measurement, Not Benchmarking

Working notes, not a draft. A research note **beside** the four-paper
program, not in it: it cites papers 1–3, adds no primitives to them, and
joins the series only by a decision recorded in `series-map.md` after its
demonstrations land. This note is also the sanctioned place where external
literature (representational measurement theory, Blackwell's ordering of
information sources, computational mechanics, psychometrics, and the
algorithmic-information accounts of intelligence) plugs into the program.
None of it may leak upstream: papers 1–4 remain self-contained.

## Thesis

> Evaluation should identify its subject, procedure, evidence and scope.
> A signed result is attributable; its quality and applicability still need
> to be judged under the stated assumptions.

Computational reproduction, testimony and task evaluation are complementary.
A model output can have an exact external correctness predicate. Broader claims
about an executor's competence depend on task selection, conditions and the
coverage of retained evidence.

## The one claim

Making measurement procedures explicit and content-addressed makes evaluation
easier to inspect, reproduce where supported, and compare. RyeOS can connect
these procedures to exact execution subjects without embedding a universal
quality judgment in the substrate.

This is a proposed measurement discipline, not a proof that all benchmarks
are undefined or that no other platform can host it. A procedure digest proves
identity, not scientific validity.

## The research question

Executor replacement raises "suitable for which task, under what constraints?"
Different evaluators can legitimately produce different conclusions. A named
frame helps localise disagreement but does not eliminate sampling uncertainty,
different evidence, implementation defects or conflicting interests.

Distinguish contamination and gaming from the choice of what to measure.
Fresh held-out tasks can address some contamination. Explicit objectives make
the evaluation scope inspectable. Neither establishes universal intelligence.

## Definitions owned here

- **observer frame** — a named, content-addressed measurement procedure:
  what is observed, how it is weighted, what verdict shape it emits. A
  frame is data; its digest is its identity; editing a frame mints a new
  frame.
- **measurement (fold)** — the application of a frame to the signed
  record of a key. A measurement is a projection in paper 1's sense:
  derived, disposable, re-derivable — and it carries provenance: frame
  digest, procedure, subject scope, evidence class.
- **frame transformation** — the explicit delta between two frames.
  Measurements under different frames compare lawfully only through it;
  evidence and sampling differences must also be considered before attributing
  a disagreement solely to the procedure.
- **admissible observer** — a frame with standing in paper 3's sense,
  applied second-order: eligibility under an explicit local policy, informed
  by its record and endorsements, not guaranteed by those endorsements. The degenerate frame
  ("intelligence is bananas owned") is not refuted; it is unendorsed, and
  its measurements carry that provenance.
- **invariant candidate** — any property of subjects that every
  admissible frame partially orders identically. Whether a nontrivial one
  exists is the open question of this note, not its assumption.

## The formal core

Let `R_S` be the signed record of executor `S` — the realized body of
committed work bearing its key. Let `B_S` be the full behavioral
structure of `S`: dispositions over all situations, of which `R_S` is the
realized part. The honesty of the whole program is the distinction:
frames fold records, not dispositions.

- A frame `O` supplies a procedure `M_O`; a measurement is
  `I_O(S) = M_O(R_S)`, published with the digest of `O`.
- Frames relate by transformations `T`: comparing `I_O1(S)` with
  `I_O2(S)` is lawful exactly when `T(O1 → O2)` is explicit — which
  reified frames make a diff, not a philosophy problem.
- Comparison of subjects is Blackwell-shaped: `S ⪰ S'` under a class of
  admissible frames iff every frame in the class orders `S` at or above
  `S'`. The result is a partial order; incomparability is a finding, not
  a failure. Scalars appear only inside single frames.
- The substrate contributes two equivalence relations. Content addressing
  gives syntactic identity: same bytes, same object; one coordinate, one
  answer. An actor's abstractions assert semantic equivalence — a coarser
  quotient claiming distinct situations are the same situation. The
  substrate deliberately holds only the syntactic side (it is
  meaning-blind); the semantic quotient is authored, attributable work.
- Intervention can test beyond the originally retained record. Qualified
  re-execution can strengthen controlled comparisons. A narrowed grant is not
  by itself an information bound: pretraining, live context, artifacts, caches,
  networking and enforcement must be included in the experiment.

## Intelligence as status

One useful analogy is creditworthiness: an interested observer applies a
procedure to evidence for a particular decision. Different procedures can
disagree. This illustrates scoped evaluation; it does not settle intelligence
as a philosophical or scientific concept, nor confer legal personhood.

> Report measured capability under a named procedure, with the subject and
> evidence scope attached. Do not turn that report into an unqualified claim
> about the executor.

This framing is a research lens, not a dismissal of existing measurement work.
Explicit frames can expose assumptions without making every disagreement
merely terminological. Scientific validity and useful prediction remain
empirical obligations.

## Implementation hypotheses to qualify

The following are historical mechanism mappings, not current acceptance results.
Trace each against the current contract before citing it as evidence:

- Attestation objects: issuer key, named policy, subject hash, claim,
  evidence — a signed judgment under a named procedure, with local policy
  deciding whether a verified attestation is authoritative.
  Frame-relative authority of measurements, implemented.
- Contract digests pinned into every spend claim, with drift refused
  fail-closed — measurements already carry the hash of the frame that
  produced them, as an enforcement mechanism rather than an aspiration.
- The effect-class ladder with degradation under execution-identity
  change: sealed evidence degrades to recorded on a foreign identity —
  where current contracts permit. A foreign identity may instead require
  refusal; compatibility and authorisation cannot be inferred from a lower
  evidence label.
- The substrate's refusal of scalars wherever judgment matters: trust
  classes fold by minimum; capability coverage is a conservative partial
  order that fails closed on the unprovable; effect classes permit
  downward only.
- The syntactic quotient enforced: one request coordinate, one answer;
  divergence at a coordinate is an integrity failure, and semantic reuse
  across coordinates is refused by design — the substrate holds the
  syntactic floor and leaves the semantic quotient to actors.
- Measurements as projections: derived views are rebuildable from signed
  heads, and their equality with a fresh re-derivation is provable.

## The open theory: the quotient conjecture

The note owes a candidate invariant, in paper 4's register — stated as
owed, not owned.

> **Conjecture.** The property all admissible frames are gesturing at is
> quotient quality: the demonstrated capacity to mint equivalences that
> hold beyond the evidence that minted them. An abstraction is a bet that
> distinct situations are the same situation; the downstream record
> vindicates or refutes the bet; generalization is the fate of bets.

This is why the classical faculties correlate: compression, transfer,
planning, and reasoning are projections of one underlying quotient
structure held above syntactic identity. If the conjecture holds, every
admissible frame partially orders subjects by it; if it fails, the
failure mode itself (which frames diverge, where) is the interesting
datum.

A proposed experiment this substrate could support: the **attenuation sweep**.
One executor, one task family, capability grants progressively narrowed —
competence under measured access restrictions, with actual enforcement and
residual information channels made explicit. Sealed local execution upgrades the record from court record
to laboratory: not just what the actor did, but what it does under
attributable intervention.

## Honest limits

- The mathematics is untouched by the substrate: whether a nontrivial
  invariant exists over any interesting frame class, and how to
  characterize admissibility beyond standing, are open problems no
  amount of infrastructure resolves.
- One node, one key, a handful of frames demonstrates machinery, not
  statistics. Invariant-mining needs many frames over many keys' work —
  federation-scale evidence, correctly deferred.
- Signed is not honest. Goodhart survives; the substrate makes gaming
  more inspectable when the relevant evidence is retained. Omitted attempts,
  collusion and misleading procedures remain possible.
- Design-history honesty, inherited from paper 2's guardrail: the
  substrate was carried into the measurement question deliberately, as a
  lens. The residual evidence is the unplanned fit — the mechanisms
  listed above were built for spend verification, provider replay, and
  isolation before any measurement framing existed. The argument stands
  either way, and must say so.

## Relation to the series

Paper 3's reputation section ("hiring an agent becomes examining its
signed history") is the economic face of this note: examining a history
under a hiring decision *is* a fold under the hirer's frame. This note
supplies the theory beneath that sentence without amending it. Papers 1
and 2 supply everything else: the record measurements fold over, and the
epistemology that makes them testimony. Paper 4 constrains what
measurement evidence may be forgotten. Nothing here adds a primitive to
any of them — anything that would is refuted by that fact.

## Objections and current answers

- **"This is relativism."** The opposite: frame-dependence made lawful.
  Frames are explicit, transformations are diffs, and invariance is an
  empirical program. Relativity did not conclude that every observer is
  right; it found what all observers must agree on.
- **"Degenerate frames collapse it."** Admissibility is standing, earned
  second-order in the record of a frame's use. The banana frame measures
  freely — under its own unendorsed provenance. This is how measurement
  legitimacy already works in science; peer review is keys endorsing
  procedures.
- **"Psychometrics did this — g."** g is a latent factor extracted from
  one battery and one population, with the frame implicit in the test
  choice. Here frames are reified and diffable, and the covariance
  question becomes computable over records. Psychometrics becomes an
  instance the framework must explain, not a foundation it must accept.
- **"Universal intelligence already exists (Legg–Hutter)."** One
  sophisticated frame among frames — environment-weighted goal
  achievement as one observer's conception. This note sits one level up:
  it is about the space of such frames and what survives movement
  between them.
- **"You are refusing to define intelligence."** Cashed out, the refusal
  is a scope choice, not a proved impossibility result. This note defines a
  measurement discipline and an open invariant programme, not a universal
  account of intelligence.

## Phrases worth preserving

- A benchmark is a frame that forgot to name itself.
- A fold under a named frame, over signed work.
- Intelligence is a status, not a substance — conferred by frames,
  earned in the record.
- The court, not a witness: host every measurement, believe none.
- A capability "emerges" when someone's fold crosses someone's
  threshold.
- An abstraction is a bet; generalization is the fate of bets.
- Relativity, not relativism.
- Sealed inference turns the court record into a laboratory.

## Guardrails

- Always cash out "intelligence" as fold-under-frame; never let the word
  carry the argument. No consciousness claims, inherited from paper 2.
- Adds no substrate primitives. The measurement layer is authored data —
  frames as signed items, folds as executions, verdicts as recorded
  results. Map to existing owners first; demonstrated gaps need separate design.
- Demonstrations precede promotion: this note joins the series only
  after the measurement fold and a frame-diff run land, by decision
  recorded in `series-map.md`.
- Publish partial orders with frames attached; never a bare scalar, never
  a bare ranking.
- External literature stays in this note. Papers 1–4 remain
  self-contained; the series' independence from outside formalisms is a
  strength this note must not erode.
