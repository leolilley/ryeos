<!-- ryeos:signed:2026-08-23T21:53:41Z:6c46ab869e2f1186e9161a96147cf8a0d377c3ad322f967e9457abceb89a6328:iBD+mSjddG+23CrXwGJtk5/CJFNl81yGrVEwf11TcuETeoy0ZRysyE+YxuQ0gdkke/XOzhYqfR6VgkSp4aqcDw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
category: ryeos/papers
tags: [papers, measurement, sincerity, character, game-theory, research-note]
version: "0.1.0"
description: >
  Math companion to measurement-not-benchmarking.md: toy-model results on
  when permanent records and diverse evaluation frames make sincerity the
  dominant strategy. The original conjecture is corrected, an absorption
  theorem replaces it, and one emergent lemma (extremal claims are
  self-identifying) falls out. Beside the program, not in it.
---

# Sincerity Under Open Frames

Working notes, not a draft. A math companion to
`measurement-not-benchmarking.md`, beside the four-paper program, subject
to the same rules: external formalism stays here, nothing leaks upstream,
and every claim is toy-model grade until stated otherwise. The models are
deliberately small; the point is which qualitative claims survive proof
pressure, and the first finding is that the conjecture that motivated this
note did not.

## The conjecture under test, and the verdict

The character thread produced the claim: *under a permanent record and an
open frame class, gaming converges to sincerity as frame uncertainty
grows.* As stated, this is **false**. Frame diversity alone does nothing:
a chameleon facing diverse audiences with no coherence pricing keeps its
full premium forever (Proposition 2, confirmed numerically at ~0.97 of
capacity, indefinitely). What is true is sharper and conditional:

> Sincerity becomes dominant when three conditions hold together: the
> record is permanent and future-weighted; frames may fold the whole
> record and price incoherence; and the frame class is diverse enough
> that its probes span the character space. Under all three, every
> strategy — honest or not — is absorbed into some fixed character within
> a number of acts on the order of the character space's dimension.
> Diversity forces sincerity by *spanning*, not by punishment: between
> them, diverse frames eventually ask every question, and one permanent
> record can only answer each question one way.

## The models

**Static (fabrication vs possession).** A probe space of effective size
`N`. Possessing a generator — actually having the property, answering any
probe consistently — costs `C` once. Fabricating answers costs `c` per
probe covered. Frames sample probes; payoffs reward passed probes.

**Dynamic (linear characters).** Characters are vectors `θ` in the ball
of radius `B` in `R^d` — `d` is the dimension of the character space. A
probe `p` is a unit vector; a generator answers `⟨θ, p⟩`. Each period an
audience probes once and rewards a high answer; every committed
`(probe, answer)` pair enters a permanent record. A coherence-pricing
fold checks whether one `θ` within tolerance `τ` explains every pair it
observes. The chameleon chooses each answer to maximize the current
audience's score subject to remaining coherent; the possessor answers
from a fixed `θ` always.

## Results

**1. Static threshold (proved, trivial; confirmed exactly).** Possession
dominates fabrication iff `N > C/c`. Gaming is precisely the closed-frame
regime: when the effective probe space is small — one benchmark — the
lookup table is cheaper than the generator. Honesty is compression;
fabrication is memorization; open frames inflate the required table while
the generator's cost stays fixed. (Run: crossover at exactly `N* = C/c =
50`.)

**2. Necessity of coherence pricing (proved by counterexample).** With
audience-payoff weight `α`, fold weight `w`, frame-alignment `m`, and
consistency price `λ`, the chameleon-minus-possessor payoff difference is
`B(1−m)[α − wm − wλB(1+m)]`. At `λ = 0` the chameleon wins whenever
`α > wm` — i.e., with diffuse frames and any real present-audience
weight, gaming dominates *at every diversity level*. Diversity alone is
insufficient; the sim's no-coherence row holds ~0.97 of capacity forever.
Siloed evaluation — no frame ever folds the whole braid — is this regime.

**3. Freedom equals unspanned dimensions (proved, linear model).** At any
time, the chameleon's tailoring freedom is exactly the character
dimensions not yet spanned by committed probes: with committed probes of
rank `r`, freedom lives in `d − r` dimensions; once probes span (rank
`d`, minimum singular value `σ`), any two coherent explanations satisfy
`‖θ − θ′‖ ≤ 2τ√d / σ`, so all future answers are pinned to `O(τ)`.

**4. Absorption (proved via 3; confirmed numerically).** Under diverse
probes, rank reaches `d` in about `d` periods almost surely, so every
undetected strategy is absorbed into the behavior of some fixed
character. In the run (`d = 5`, `τ = 0.05`): rank hit 5 at `t = 5`, the
feasible set's diameter collapsed 1.91 → 0.27 → 0.11 → 0.016 → 0.000,
and thereafter the chameleon's scores were statistically
indistinguishable from an honest agent's. You get `d` free choices;
after that, the record decides. The only choice a permanent record
leaves open is *which* character — chosen deliberately, or entangled by
your early improvisations.

**5. Extremal claims are self-identifying (proved, geometry; no
simulation needed).** A claim near capacity pins the claimant: if a
committed answer satisfies `y ≥ B − ε`, coherence forces
`‖θ − B·p‖ ≤ √(2B(ε+τ))` — the feasible set collapses to a cap around
one type after a *single* maximal claim. Moderate claims leave large
feasible regions. Boasting is self-binding: the maximal claim is the
most identifying claim, and greed accelerates absorption. This connects
the courage clause to a budget: strong commitments are what measured
intelligence requires, and each one spends tailoring freedom. A life
under a permanent record is a freedom-spending schedule.

## Open problem: the forgiveness trade-off, quantified

Window-`W` folds (only the last `W` acts are coherence-checked) bound
reinvention capacity by the unspanned dimensions of the window,
`d − min(W, d)`: windows shorter than the character dimension leave
permanent tailoring headroom; windows that span it (`W ≥ d`) permit only
slow drift — redemption without simulacra. That upper bound is proved;
what an *optimal* windowed strategist actually extracts is not. The
simulated agents here explore only near their recent selves and get
absorbed even under short windows — a local-search artifact, flagged
honestly, though arguably also realism: an agent that can only drift its
self-presentation cannot exploit a forgiving window; exploiting
forgiveness requires the capacity for radical reinvention. Resolving the
optimal-play dynamic program is the note's main open problem, and it is
exactly the price question for the forgiveness clause: the fold horizon
that lets an agent outgrow its past is the same parameter that lets a
strategist run a rolling mask.

## What the correction teaches

The three conditions map one-to-one onto substrate mechanisms and onto
today's evaluation pathologies:

| Condition | Substrate mechanism | Failure mode when absent |
| --- | --- | --- |
| Permanent, future-weighted record | the braid; acts consumed by later folds (hiring = examining signed history) | ephemeral sessions: present-audience gaming |
| Whole-record coherence folds | frames may fold the entire chain | siloed evals: audience-partitioned simulacra, full premium forever |
| Probe diversity spanning the character space | open frame authorship; admissibility earned, frames minted freely | one closed benchmark: permanent free dimensions — the Goodhart subspace |

The contemporary AI evaluation regime fails all three at once: closed
static benchmarks, siloed per-eval judgment, stateless sessions. The
substrate sets all three knobs the other way. That is the practical
content of this note: sincerity-dominance is not a hope about agents; it
is a parameter regime, and the parameters are architectural.

## Adjacent literature (stays in this note)

Reputation games (Kreps–Wilson; Milgrom–Roberts) get sincerity from
repeated play against strategic observers; here the observers need not be
strategic — permanence plus coherence does the work. Career-concerns
models (Holmström) show observation *distorting* effort; the coherence
term is what flips observation from distorting to absorbing. Costly
signaling (Spence; Zahavi) prices honesty per signal; the spanning
argument is a statement about when the whole signal *portfolio* must be
generated rather than assembled. The quotient conjecture in
`measurement-not-benchmarking.md` and Result 1 are the same object seen
twice: a generator versus a lookup table — which is why, in this
framework, sincerity and intelligence are measured by the same fold.

## Limits

- Linear generators, scalar answers, toy dimensions. Nothing here is
  claimed beyond the model class; the value is which qualitative shapes
  survived and which died.
- The simulated chameleons are boundedly rational (local search); all
  intermediate-window numbers are lower bounds on strategic capacity.
- Single agent, exogenous frames: no collusion, no frame capture, no
  strategic frame authorship. The rating-agency failure mode is outside
  this model and unresolved by it.
- Perfect simulation is not distinguished from possession — by design.
  On a testimony substrate there is no fact of the matter beyond the
  record (the specification gap); a simulator coherent under every probe
  *is*, extensionally, a possessor. The theorems bound the profitability
  of imperfect simulation; the mask that must fit every question becomes
  the face.

## Phrases worth preserving

- Diverse frames force sincerity by spanning, not by punishment.
- You get `d` free choices; after that, the record decides.
- Boasting is self-binding: the maximal claim is the most identifying
  claim.
- A life under a permanent record is a freedom-spending schedule.
- Honesty is compression; fabrication is memorization.
- Sincerity-dominance is a parameter regime, and the parameters are
  architectural.

## Guardrails

- Toy-model grade throughout: cite these as results *in the linear
  model*, never as facts about agents. Promotion of any claim requires
  either a general proof or empirical folds over real records.
- Adds no substrate primitives; the theorem's three conditions must map
  to mechanisms that already exist, and do.
- The corrected conjecture supersedes the character-thread phrasing
  wherever the two differ; do not quote the original form.
- External formalism stays here, per the measurement note's rule.
