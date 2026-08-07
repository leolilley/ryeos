<!-- ryeos:signed:2026-08-07T08:08:24Z:59df498ef517d861127d8167c4e9ef1d0a8e4445a9be435e25f145505e87c6ff:Nb4OzQCSdZ3bBUjcn5cj5i4gWRsalHI06C0rNN5I/OHpPsOuVgvzk7ujawdjXIq0LiHV9zbIZOCvNAOI0vrfBA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, determinism, replay, provider, directive, evidence]
version: "0.1.0"
status: draft
description: >
  Recorded-class effect records for provider (LLM) calls at the directive
  boundary — the remaining large surface between "graph nodes replay" and
  "a no-change re-run is free end to end."
---

# Provider-call effect records

Durable node effect records made deterministic graph nodes replay across
runs. The turns a directive spends against a provider are the surface that
does not: every re-run re-pays every LLM call, so a campaign-instrument
re-solve is nearly free *except* for the thing that dominates its cost. This
note designs the recorded-class boundary for provider calls. It is the
determinism-classes contract applied to its largest consumer, not a cache
bolted onto a transport.

## What the record is

One provider call is one recorded-class effect: identity is the exact
request, the record is the exact response.

- **Identity**: a schema-tagged canonical digest over everything that
  selects the response distribution — resolved model id, provider route,
  sampling params, tool definitions, and the complete message sequence.
  Sampling nondeterminism is irrelevant to identity: `recorded` captures a
  nondeterministic effect at its boundary; the *request* is the key, the
  response is evidence, and equal keys serve the same evidence.
- **Scope**: keyed under the run's effective definition digest exactly like
  node records — no cross-digest reuse, ever. A changed directive, tool
  set, or realization moves the digest and every key under it.
- **Chaining**: turn N's messages contain turn N-1's recorded response and
  the recorded results of tool calls between them, so replay composes
  forward on its own: identical history yields identical keys until the
  first divergence, and the first miss names the frontier where behavior
  moved. No turn counter is needed in the key — history is the counter.

## The placement problem (the open decision)

Provider transport lives in the directive runtime subprocess
(`runtimes/directive/src/provider_adapter/`), inside the sandbox. The node
effect-record law — authority never enters the sandbox; the daemon owns the
store — cannot be satisfied by pointing the existing machinery at this
boundary. Three candidate placements:

1. **Daemon proxy** — provider calls route through the daemon, which
   records and replays exactly like node dispatch. Cleanest authority
   story; largest change; puts a streaming hot path through the callback
   socket.
2. **Runtime capture, daemon publication** — the runtime submits
   request-digest + response through the existing callback boundary; the
   daemon validates against its own accounting observation of the same
   call before publishing. Binds the record to independently observed
   usage, so a lying runtime cannot bank a response the provider never
   returned. Accounting already crosses this boundary (provider budget
   reservation); the record rides an existing crossing.
3. **Index existing turn evidence** — directive turns already persist as
   durable thread events (chained resume replays them). If the persisted
   turn evidence carries the full request/response pair, records become an
   *index* over evidence that already exists, and capture costs nothing
   new. Replay then needs only the serve path.

Leaning: (3) for capture if the persisted turn shape proves sufficient,
with (2)'s validation posture for publication; (1) only if streaming proves
the others unworkable. The decision needs a code-level audit of what turn
events durably carry today — that audit is increment 0.

## Replay semantics

- Opt-in and sealed, mirroring node declarations: a directive (or its
  kind) declares its provider boundary `recorded`; default `live` keeps
  every existing directive's sampling semantics untouched. A user asking a
  fresh question expects fresh sampling; a campaign re-solve expects the
  record. The declaration composes and seals like everything else.
- A replayed call bills nothing, consumes no reservation, and carries
  `replayed_from` provenance in the turn evidence — the same honesty rule
  as node receipts: never a false "the provider said."
- Streaming replays as the recorded final message. Chunk cadence is
  transport texture, not evidence; anything that *observed* the stream
  live is by definition not replaying.
- The author covenants hold unchanged: a provider call is result-complete
  by construction (its only effect is its response), and message content
  must be run-stable — a timestamp interpolated into a prompt is the
  run-scoped-params footgun with a token bill attached.

## Retention

Records are large (full conversation prefixes) and highly redundant across
turns. Store messages as content-addressed blobs so shared prefixes
deduplicate structurally in CAS; the record object holds hashes, not text.
Same never-replayed-first pruning lanes as node records; same honest-loss
rule — an evicted record means the next run pays the provider and is a
different run.

## Non-goals

- No semantic caching — similar prompts are different requests.
- No cross-digest or cross-node reuse, no distributed sharing.
- No replay of genuinely interactive turns (user input mid-run is `live`
  by definition).

## ARC payoff

With node records alone, a no-change re-solve replays measurement and
simulation but re-pays every reasoning turn. With provider records, the
no-change campaign re-run is free end to end — the 100%-replay proof
includes the solver's reasoning — and a divergence localizes to the first
turn whose request digest moved, which names the exact upstream change
(tool result, realization, directive text) that altered what the model was
asked.

## The local endgame

This boundary's `recorded` ceiling is a property of *remote* providers,
not of LLM calls. Local inference on tinygrad under full execution
control upgrades the class to `sealed` — weights, kernels, sampler state,
and device as admitted content, re-derivation instead of replay, token-
level checkpoints, KV-prefix reuse as CAS. The destination design is
`knowledge:ryeos/future/sealed-local-inference`. Everything here is built
to carry over: the request-identity scheme and the record store are
placement- and class-agnostic, so local arrival changes the class marker
on the same keys rather than replacing the machinery.

## Increments

0. Audit what directive turn evidence durably carries today; decide
   placement (3) vs (2) vs (1) from what exists rather than from taste.
1. Canonical request identity (schema-tagged digest, shared derivation on
   both sides of whatever boundary placement selects).
2. Record object + index (the node-record pattern: CAS object under the
   pinned authority, operational index rows as GC roots, shared retention
   lanes).
3. Capture for non-streaming calls behind the sealed opt-in declaration.
4. Replay serve with provenance, billing nothing, consuming no
   reservation.
5. Streaming capture/replay.
6. Measure on ARC: a full re-solve of a solved game under an unchanged
   digest, reporting replayed turns, saved spend, and wall-clock delta —
   the end-to-end-free number the campaign instrument exists to produce.
