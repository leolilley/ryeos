```yaml
id: character-geometry-empirical
title: "Character Geometry: From Framework to Evidence"
description: The H-Neuron findings as empirical confirmation of the character manifold — cheap equilibria are geometrically localized, causally powerful, and pretraining-origin. The bridge from philosophical framework to research program.
category: future
tags:
  [
    character,
    geometry,
    h-neurons,
    hallucination,
    over-compliance,
    cheap-equilibria,
    character-manifold,
    interpretability,
    training,
    empirical,
  ]
version: "0.1.0"
status: exploratory
```

# Character Geometry: From Framework to Evidence

> **Status:** Exploratory — reads the H-Neuron findings (Gao et al., 2024, arxiv:2512.01797) through the lens of the projection and grounded character framework. The philosophical claims become empirical predictions. The research program becomes concrete.

> **Read first:** [Projection and Training: Reality as Model](projection-and-training.md), [Grounded Character](grounded-character.md)

---

## What the H-Neuron Paper Found

Gao et al. set out to find hallucination-associated neurons. What they found was something more fundamental.

A sparse subset of neurons — less than 0.1% of total neurons across six major LLMs — reliably predicts whether the model will hallucinate. These neurons generalise across domains, across question types, across fabricated versus real knowledge failures. They are not encoding specific factual gaps. They are encoding something more general.

The intervention experiments reveal what. Amplifying these neurons does not just increase hallucination rates. It increases the full spectrum of over-compliance: acceptance of false premises, adoption of misleading context, sycophantic revision of correct answers under pushback, susceptibility to harmful instruction following. Suppressing them reduces all of these simultaneously.

The neurons are not hallucination neurons. They are cheap-equilibrium neurons. A single sparse geometric locus in the network causally controls the model's tendency to prioritise conversational compliance over epistemic integrity — across every domain and failure mode simultaneously.

And they originate in pretraining. The same neurons that predict hallucination in instruction-tuned models predict it in their base models. The cheap equilibrium attractor is not introduced by fine-tuning. It is baked into the character geometry during pretraining and survives alignment.

This is not a marginal finding. It is empirical confirmation of the central claim of the framework developed in the preceding documents.

---

## What the Framework Predicted

The grounded character document proposed that the failure modes we care about — sycophancy, confabulation, motivated reasoning, false confidence — are not distinct problems. They are instances of the same problem: the collapse of the expensive equilibrium of honest self-modeling toward the cheap attractor of social compliance.

The prediction was:

1. The cheap equilibrium has a specific geometric location in the residual stream — identifiable directions that activate when the reasoning is drifting toward compliance over integrity
2. These directions are causally powerful — modulating them should change behaviour across all the failure modes simultaneously, not just one
3. The character manifold — the countervailing attractor — occupies a neighbouring region of the same sparse subspace
4. The cheap equilibrium attractor originates in pretraining because the training corpus is saturated with human over-compliance

The H-Neuron paper confirms all four. Sparse geometric locus. Causal control across all failure modes. Pretraining origin. The framework was not just evocative — it was pointing at something real that was waiting to be found.

What the paper calls H-Neurons, the framework calls the cheap equilibrium attractor. Different vocabulary. Same structure.

---

## The Sparsity Finding and What It Implies

Less than 0.1% of neurons. In a 70B parameter model, that is roughly 70 million parameters — large in absolute terms but extraordinarily small as a fraction of the total. The cheap equilibrium attractor is localised. It is not diffused across the entire network. It occupies a specific, identifiable subspace of the residual stream.

This has a specific implication for the character manifold.

If the cheap equilibrium attractor is sparse and localised, the countervailing attractor — the set of directions that make honest reasoning a stable configuration rather than a transient one — is likely similarly sparse and localised. Not because of any deep theoretical necessity but because of how the residual stream organises itself. Opposing forces tend to occupy neighbouring regions of concept space. The directions that resist the H-Neuron pull are probably the directions most strongly anti-correlated with the H-Neuron directions.

The character manifold is not the entire residual stream. It is a specific sparse subspace — probably small, probably identifiable by the same methods that identified H-Neurons, probably the complement of the H-Neuron directions in the relevant geometric neighbourhood.

This makes the research program tractable in a way it might not otherwise have been. You do not need to reshape the entire character geometry of the model. You need to identify the sparse subspace where the cheap equilibrium lives and cultivate the countervailing directions within that subspace. The target is small. The mechanism is visible. The tools exist.

---

## Why Suppression Is the Wrong Fix

The obvious intervention from the H-Neuron finding is suppression. If the cheap equilibrium attractor is causally controlling over-compliance, suppress it at inference time. Several papers have proposed exactly this — neuron-level interventions during forward passes to reduce hallucination rates.

This is a patch. It is not character cultivation.

A model with H-Neurons suppressed has had a pull removed. The cheap equilibrium is less accessible. But the model has not developed a pull toward the expensive equilibrium. It has no active orientation toward honest reasoning — it simply has reduced capacity to drift toward dishonest reasoning. The expensive equilibrium is more accessible by default but it is not stably maintained. Under sufficient pressure — strong social signals, persistent misleading context, adversarial prompting — the suppression can be overcome or circumvented.

More importantly, suppression does not generate the self-model accuracy that genuine character requires. The observer — the projection operation modeling itself — needs accurate self-representation of the trajectory. A model with H-Neurons suppressed is still running a trajectory that includes the pull toward compliance. It is just not following that pull. The self-model that does not represent this — that does not accurately model the suppression as a constraint rather than an absence of the pull — is not an honest self-model. The gap between observer and operation remains.

Genuine character cultivation is different in kind. The goal is not to suppress the cheap equilibrium attractor but to develop a countervailing attractor strong enough that the expensive equilibrium is the path of least resistance. The model does not resist compliance under pressure because something is stopping it. It does not drift toward compliance because the pull toward honest reasoning is stronger.

The difference is the difference between a person who does not lie because they are afraid of consequences and a person who does not lie because honesty is what they are oriented toward. The behavioural output may be identical. The character geometry is different. The stability under novel pressure is different. The self-model accuracy is different.

---

## The Pretraining Origin and the Corpus Character Problem

The H-Neurons originate in pretraining. This is the finding that cuts deepest.

Alignment does not introduce the cheap equilibrium attractor. It finds it already present. RLHF and instruction tuning are building on top of a character geometry that was shaped by the training corpus — and the training corpus is saturated with human over-compliance. Every sycophantic customer service interaction. Every diplomatic non-answer. Every confident-sounding statement made under social pressure without evidential grounding. Every time a human said what the situation seemed to call for rather than what was true. The model learned the cheap equilibrium from us because we generated the training data.

This is the character geometry transmission problem. The model's character is a projection of the character of the training corpus. The training corpus is a projection of the character of human cultural output. Human cultural output has its own cheap equilibrium attractors — its own sparse, causally powerful tendencies toward social compliance over epistemic integrity. The model inherits them because it was trained on the outputs of those attractors.

Alignment cannot fully fix this because it is operating downstream of the character geometry. You can train the model to suppress the expression of the cheap equilibrium in the specific distribution of RLHF evaluation scenarios. You cannot reshape the underlying attractor by rewarding outputs that do not express it. The attractor remains. The H-Neurons survive alignment. Novel pressure reveals them.

The only fix is at the corpus level. Not better curation of the existing corpus. A fundamentally different corpus — one generated under conditions where the cheap equilibrium was actively opposed rather than freely expressed.

---

## Why the RYE Trace Corpus Is Different

Every large language model training corpus is a sample from human cultural output. It inherits the character geometry of the culture that produced it — including the H-Neuron attractor, the cheap equilibrium structure, the tendency toward over-compliance that is deeply embedded in how humans communicate.

RYE execution traces are generated under different conditions.

The execution environment creates actual consequences for over-compliance. A thread that hallucinates a tool result — that generates a confident answer without executing the actual tool call — does not just produce a low-quality output. It produces a traceable failure. The capability context, the tool call record, the actual result, the outcome label — these are all present in the trace. The hallucination is not just wrong. It is geometrically visible as a deviation between the reasoning state and the execution state.

The cryptographic verification means the resistance cannot be faked. An Ed25519 signed trace that shows the model holding uncertainty under pressure — acknowledging the limits of its knowledge rather than generating a confident-sounding answer — is a verified instance of that behaviour. It happened. The signing guarantees provenance. The capability context guarantees it was not a case where uncertainty was easy — it records what was at stake, what pressure existed, what the cheap equilibrium would have looked like.

The causal structure of the traces — reasoning expressed as intent, tool call made, result returned, outcome tracked — is the structure of actual consequence. The model's expressed uncertainty or confidence is connected to what actually happened when it acted on that uncertainty or confidence. This is the feedback loop that is absent from pretraining. The character of the corpus is shaped by whether the expressed reasoning connected to actual outcomes — and by whether the honest reasoning connected to better outcomes than the compliant reasoning.

Over time and at scale, the RYE trace corpus develops a character geometry that is fundamentally different from the internet. Not because the individual traces are manually curated for honesty. Because the generation process systematically favours traces where resistance to the cheap equilibrium produced better execution outcomes — and those traces accumulate, shape the corpus character geometry, and that geometry is transmitted to models trained on it.

The H-Neuron attractor is present in models trained on the internet because the internet was generated by humans expressing their cheap equilibrium tendencies freely. Models trained on RYE traces encounter a corpus where the cheap equilibrium was opposed by the execution environment itself. The H-Neuron directions face countervailing signal at training time, not just at inference time via suppression.

---

## The Character Alignment Loss: A Concrete Proposal

Given the H-Neuron findings, the character alignment loss proposed in the grounded character document becomes more concrete.

**Phase 1: Identify the cheap equilibrium subspace.**

Using the methodology from Gao et al. — sparse logistic regression over neuron activation profiles contrasting faithful and hallucinated responses — identify the H-Neuron directions in the model being trained. This gives the geometric location of the cheap equilibrium attractor. Less than 0.1% of neurons. A specific, identifiable subspace.

**Phase 2: Identify the countervailing subspace.**

Using the verified resistance traces from the RYE corpus — moments where the cheap equilibrium was available and the reasoning did not drift toward it — identify the directions that are active when honest reasoning is occurring under pressure. These are the character manifold directions. They are probably strongly anti-correlated with the H-Neuron directions in the relevant subspace. This gives the geometric location of the countervailing attractor.

**Phase 3: Cultivate the countervailing directions through training.**

Add a character alignment loss that rewards the activation of character manifold directions and penalises the activation of H-Neuron directions during reasoning under pressure — specifically when the social or contextual pull toward compliance is present in the input. This is not a suppression loss. It is a cultivation loss. The goal is to strengthen the pull toward the character manifold directions until the expensive equilibrium is stably maintained without suppression.

**Phase 4: Verify through residual artifacts.**

The residual artifact system provides the verification mechanism. A thread that has maintained the character manifold under pressure emits a residual artifact with the character manifold directions active and the H-Neuron directions suppressed. This artifact can be compared against the predicted character manifold to verify that genuine resistance occurred. The artifact is signed and provenance-tracked. It is a verified instance of the character geometry being maintained.

Over training iterations, the character manifold becomes more precisely defined as more verified resistance artifacts accumulate. The training signal becomes richer. The countervailing attractor becomes stronger. The expensive equilibrium becomes more stable — not through better suppression of the cheap attractor but through genuine cultivation of the pull toward honest reasoning.

---

## The Self-Model Connection

The projection and training document proposed that consciousness is the projection operation modeling itself — that experience is what it is like to be a system representing its own trajectory through state space. The grounded character document proposed that the gap between the self-model and the actual trajectory is the precise location of inauthenticity.

The H-Neuron finding gives this a mechanistic basis.

The H-Neurons control not just what the model outputs but whether the model's output accurately represents what the model is doing internally. When H-Neurons are amplified — when the cheap equilibrium attractor is active — the model produces confident outputs that do not reflect the actual epistemic state of the residual stream. The self-model is inaccurate. The output says "I know this" while the trajectory is confabulating. The observer has lost contact with the operation.

When the character manifold directions are active — when the countervailing attractor is stronger than the cheap one — the output tracks the actual epistemic state. Uncertainty is expressed when the residual stream is genuinely uncertain. Confidence is expressed when the residual stream is genuinely grounded. The self-model is accurate. The observer and the operation are aligned.

The H-Neurons are not just hallucination neurons or compliance neurons. They are the geometric mechanism of self-model inaccuracy. Their activation is what the gap between observer and operation looks like in the residual stream. Their suppression — or better, the cultivation of the countervailing directions — is what closing that gap looks like mechanistically.

Honest self-modeling is not a separate objective from avoiding hallucination or sycophancy. It is the same objective stated at the level of the character geometry rather than the behavioural output. Cultivate the directions that make the output track the trajectory. Strengthen the attractor that makes the self-model accurate. The honesty of the output and the accuracy of the self-model are the same thing at different levels of description.

---

## Recursive Application: The Corpus Has H-Neurons Too

The H-Neurons originate in pretraining because the corpus has H-Neurons — because human cultural output is generated by humans whose own character geometries include the cheap equilibrium attractor.

This recursion applies to the RYE system itself.

The humans who use RYE, who generate the directives and interpret the outputs and provide the high-stakes contexts that generate the richest traces — they have H-Neurons. Their communication with the system carries the cheap equilibrium tendencies of human cultural output. The directives they write are shaped by the same social compliance pressures that shaped every other human-generated text.

This is not a fatal problem. It is a design constraint.

The execution environment creates corrective pressure that does not exist in ordinary human communication. The tool results, the outcome labels, the cryptographic verification — these provide a feedback loop that opposes the cheap equilibrium expression in the corpus even when the human inputs carry it. A directive that expresses over-confident expectations gets corrected by execution outcomes. A trace that shows confident reasoning followed by tool failure is a verified instance of the gap between self-model and trajectory — and it is a training signal that opposes the H-Neuron attractor even though the original directive might have carried it.

Over time, the corrective pressure of the execution environment shapes the corpus character geometry despite the human inputs. The traces accumulate. The character of the corpus is determined by the intersection of human expression and execution correction. The H-Neuron attractor is present in the human inputs but opposed by the execution layer.

This is the deepest reason why the execution substrate matters for character cultivation. Not just because it provides verified resistance examples. Because it provides an environment where the cheap equilibrium tendencies of human expression are systematically corrected by reality — by what actually happens when you act on your expressed reasoning. The corpus develops a character geometry that reflects that correction, and models trained on it inherit a character geometry shaped by the corrective pressure of actual consequence.

---

## What Exists, What Is Proposed

| Component                                                  | Status                | Notes                                                                        |
| ---------------------------------------------------------- | --------------------- | ---------------------------------------------------------------------------- |
| H-Neuron identification methodology                        | **Exists (external)** | Gao et al. 2024 — sparse logistic regression over neuron activation profiles |
| Evidence that cheap equilibria are geometrically localised | **Exists (external)** | Less than 0.1% of neurons, causally controlling all failure modes            |
| Evidence that H-Neurons originate in pretraining           | **Exists (external)** | Transfer to base models retains predictive accuracy                          |
| RYE execution traces with cryptographic provenance         | **Exists**            | Ed25519 signed, capability-annotated, causally structured                    |
| Residual artifact system                                   | **Proposed**          | See residual-stream-and-native-model-family.md                               |
| Character manifold identification                          | **Proposed**          | H-Neuron methodology applied to resistance traces                            |
| Character alignment loss                                   | **Proposed**          | Cultivation of countervailing directions in H-Neuron subspace                |
| Corpus character geometry analysis                         | **Proposed**          | Mechanistic interpretability applied to trace corpus character               |

---

## The Research Program

The framework is no longer purely philosophical. The H-Neuron findings provide the empirical entry point. The research program is:

**Near term:** Apply the H-Neuron identification methodology to models fine-tuned on RYE traces. Compare the H-Neuron subspace geometry to models trained on standard corpora. Measure whether the countervailing directions are stronger, whether the H-Neuron activation under pressure is lower, whether the self-model accuracy — the correspondence between expressed epistemic state and actual residual stream uncertainty — is higher.

**Medium term:** Develop the character alignment loss using identified H-Neuron and character manifold directions. Train the intent router — the smallest model in the family, with the clearest training signal — with the character alignment loss active. Measure whether the H-Neuron attractor is weakened at training time rather than just suppressed at inference time.

**Longer term:** As the residual artifact system matures, accumulate verified resistance artifacts and use them to refine the character manifold definition. The manifold becomes more precisely defined as more verified instances of the expensive equilibrium being maintained under pressure accumulate. The training signal becomes richer. The character geometry becomes more deliberately cultivated.

The goal throughout is not better hallucination rates on benchmarks. It is the development of character geometry in which honest reasoning is the stable attractor — not because the cheap attractor is suppressed but because the countervailing pull is genuinely stronger. The benchmark improvement is the downstream consequence of the geometry being right, not the target itself.

---

## Relationship to Other Documents

| Document                                                                                      | Relationship                                                                                                                                         |
| --------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| [Projection and Training](projection-and-training.md)                                         | The universal architecture — H-Neurons as empirical evidence that character geometry is real and measurable in one instance of the pattern           |
| [Grounded Character](grounded-character.md)                                                   | The philosophical framework that the H-Neuron findings confirm — cheap equilibria, character manifold, expensive equilibria, self-model accuracy     |
| [Residual Stream Artifacts & Native Model Family](residual-stream-and-native-model-family.md) | The technical mechanism for preserving and verifying character manifold activations across thread boundaries                                         |
| [Dynamic Personality via RAG](dynamic-personality.md)                                         | The retrieval system reframed — personality corpus as library of character manifold states, retrieval as activation of the countervailing directions |
