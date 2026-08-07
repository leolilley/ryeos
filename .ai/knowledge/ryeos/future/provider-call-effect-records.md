<!-- ryeos:signed:2026-08-07T09:49:08Z:230f79e067c921f4da82d07e33560574206a74bcf95677a15b3e6d688a6731e5:gAIELO9eUn46ibC3JxDX/XQ/HO8yiEcdzemX0J2bW7vaNdIy9OwnZWUX+1CmNzHyjEp7bAF59GfLG8EMRw/2Dw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
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

**DECIDED (increment-0 audit, 2026-08-07): placement (2), and its hard
part already exists.** The audit found:

- (3) fails on facts: the durable turn evidence (`cognition_out`) carries
  the conversational fold — assistant content, tool_calls, reasoning,
  token counts, provider accounting — not the request as sent. Resume
  re-derives requests from the sealed program; a record identity must be
  computed over what was *actually* sent, or a request-assembly bug
  becomes an identity the instrument can never see.
- (2)'s identity and validation crossing are already built. The
  spend-bound machinery prepares one immutable request — exact body
  bytes, digested once (`PreparedProviderRequest.request_digest` over
  method, url, header names, `body_sha256`, output ceiling; credential
  value excluded) — and the reserve RPC already carries that digest to
  the daemon, which durably retains it as a NOT NULL ledger column.
  Record publication therefore binds to the reservation row: no
  reservation with that digest, no record, and settlement reconciles
  usage on the same row. A lying runtime cannot bank a response for a
  request the daemon never saw reserved.
- (1) is unnecessary: no proxy, the streaming hot path stays put.

What capture still needs: the response-as-consumed (the folded event
fields are a projection of it), submitted through a new record-publication
callback validated against the reservation ledger. Stored request bytes
are optional CAS blobs for divergence forensics only — replay never needs
them, because the runtime re-prepares and re-digests, and the digest match
IS the identity check.

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
- Records exist only on ledger-backed routes (first live smoke,
  2026-08-07): publication binds to the accounting reservation, so an
  advisory-only route banks nothing and the runtime warns at
  construction. Financially attributed identity is part of the record's
  proof, not an inconvenience — a route worth replaying is a route worth
  accounting for. A replayed turn is likewise exempt from the
  fail-closed usage snapshot: it has no usage by construction, and its
  accounting fact is `replayed_from` in the durable turn evidence.

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

0. **DONE (2026-08-07)** — audited what turn evidence durably carries;
   placement decided: (2), riding the existing reservation crossing (see
   the placement section for the findings).
1. **DONE with 2** — the identity collapsed to reuse as predicted: the
   daemon-side `provider_call_cache_key` is a schema-tagged digest over
   (effective definition digest, request digest), and the record's
   validation requires its cache key to answer for its own identity
   fields, so a forged record cannot even decode.
2. **DONE (`3c4ffd2de`)** — `provider_call_effect_record` CAS object
   (4 MiB response ceiling, class-gated, provenance and settled
   accounting carried) plus `provider_call_records` in the operational
   ledger as schema v3: forward migration shares its DDL verbatim with
   fresh init under the byte-for-byte assertion, a v1 store chains
   through both migrations in one open, rows feed both GC gatherers as
   roots, and retention reuses the never-replayed-first lanes.
3. **DONE** — capture behind the sealed opt-in declaration, both halves.
   Runtime half (`e3c014751`): after a settled, completed call, a
   directive whose sealed program declares a durable class submits the
   intent and envelope preimages, body digest, and response-as-consumed
   through `runtime.publish_provider_call_record`; failure is record
   loss, never turn failure. The directive kind's extends-chain composer
   and composed contract carry the top-level `effects` field so the
   declaration seals into the digest the daemon reads. (Capture runs
   after stream completion, so "non-streaming first" arrived as
   final-response capture; increment 5 owes only replay-into-a-stream.)
   Daemon half (`a669df5ec`):
   `runtime.publish_provider_call_record` validates preimage-first — the
   echoed reservation intent must hash to the ledger's stored
   `request_hash` for a settled attempt owned by the publishing thread
   (collision resistance binds the body digest to a billed reservation;
   no accounting schema change), the class is read from the admitted
   capsule's sealed `effects` declaration, and both digests are
   daemon-recomputed from the shared derivations now living in
   `ryeos-accounting`. Remaining for 3: the runtime-side submission after
   settle and the directive kind's `effects` admission treatment.
4. **DONE (`4ff41a5b4`)** — replay serves before any reservation: the
   runtime asks with the envelope preimage capture publishes, a hit
   skips reserve/issue/transport/settle entirely, the daemon refuses
   lookups from undeclared programs (no existence oracle), and the
   response reconstructs with no usage — a replayed call has no provider
   usage — while `replayed_from` provenance lands in the turn's durable
   provider accounting.
5. **DONE (`e3c014751`, `4ff41a5b4`, and the ephemeral surface commit)**
   — capture always ran post-stream-completion, and replay serves the
   recorded final message with one ephemeral delta for live watchers;
   chunk cadence was never evidence.
6. Measure on ARC: a full re-solve of a solved game under an unchanged
   digest, reporting replayed turns, saved spend, and wall-clock delta —
   the end-to-end-free number the campaign instrument exists to produce.
