<!-- ryeos:signed:2026-08-03T05:25:36Z:38250d13a4130babdbea0d19f3364f2c38ee54c6908acb7220aa2c7e80a638c7:h8VEJugiXxuVOsjxKWdKJCrzHaVOH68jaiA0ylReJ7qIUhqIMOKNXeTPQiElS4j+2sJyYNnDfqhD754ucDLUBw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/future
name: chat-latency-investigation
title: Chat Latency Investigation and Optimization Order
description: Measurement contract, observed RyeOS latency boundaries, implemented safeguards, and evidence gates for future work
entry_type: design
version: "0.2.0"
```

# Chat Latency Investigation and Optimization Order

## Status

This note records the generic conclusions from the 2 August 2026 profiling
investigation. It is not an application configuration and does not define a
provider-specific fast path.

The investigation pulled forward four provider-neutral mechanisms:

- opt-in latency-profiling builds and structured child timing records;
- bounded ordered batching of progressive runtime callbacks;
- signed provider reasoning policy with descriptor-owned wire mappings; and
- signed cumulative directive `limits.tool_calls` with ordered refusal and
  resume-safe accounting.

Deterministic parser caching and launch-attestation fast paths were also added.
They reduce RyeOS-owned work, but the samples did not demonstrate a causal
improvement in user-visible first text. Provider variance was larger than the
saved local work, so these changes must not be presented as chat-latency wins
without a paired benchmark.

Provider-neutral cache hit/miss usage mapping and an exact signed
`limits.provider_request_body_bytes` guard were added on 3 August 2026. The
cache mapping supplies authenticated evidence and correct partitioned pricing;
it does not create a provider cache. The request-body guard refuses an oversized
prepared round before ledger admission or provider contact; it prevents slow
tail drift but does not accelerate a compliant request.

## Event meanings

The latency clock begins when the daemon receives the signed launch request.
Report these boundaries separately:

1. request received;
2. planning started and completed;
3. execution admitted;
4. `stream_started`;
5. provider request submitted;
6. provider response headers;
7. first provider stream event;
8. first reasoning event, when present;
9. first non-whitespace visible assistant text;
10. every tool start and result;
11. every subsequent provider request; and
12. terminal completion or failure.

`stream_started` means that RyeOS has durably admitted the execution, published
the runtime handoff, and opened the daemon event stream. It does **not** mean
that the provider has accepted a request or produced output. Provider-level
milestones use the generic `observation` event with namespaced payload kinds
`directive.provider_response_headers`, `directive.provider_stream_started`, and
`directive.provider_reasoning_started`. These are directive-runtime semantics,
not provider-specific variants in the engine event vocabulary. A client must
not relabel runtime readiness as model activity.

Reasoning milestones expose timing only. Hidden chain-of-thought remains hidden
and is replayed internally only where a provider's tool-continuation contract
requires it.

## Measured latency model

Controlled local samples used the native provider path, remote data tools, a
profiling release build, a disposable signed project copy, and no production
deployment. The figures below are single samples or small ranges, not service
SLOs.

| Component | Observed evidence | Conclusion |
|---|---:|---|
| Warm daemon launch to runtime-ready | about 0.23 s | Already subsecond; not the chat floor. |
| Cold materialization/runtime-ready control | about 1.78 s | Cold verified-executable work can be visible once. |
| First-call DNS | about 25 ms in one captured call | Real but small. |
| First-call aggregate connection establishment | about 68 ms in that call | A worker can save only this bounded slice. |
| Later calls in the same invocation | 0 DNS/connect measurements | The current client already reuses its connection. |
| Remote data-tool execution | commonly 0.6-1.1 s in the controlled set | Material, but below provider-round latency. |
| Warm no-tool first visible text | 2.02-2.43 s in the final controls | Dominated after runtime-ready by provider inference/streaming. |
| Progressive callback time | hundreds of milliseconds on long calls | Worth controlling, not the multi-round root cause. |

A pathological analysis sample took 67.11 seconds, made four provider calls
and eleven tool calls, and grew its provider request from about 9.2 KB to
64.5 KB. The fourth request contained about 46.5 KB of tool messages. The
database calls were not the dominant term; serial provider/tool rounds and the
accumulated transcript were.

With a disposable signed three-call budget, the equivalent sample completed
in about 24.05 seconds: the first provider response requested four tools, the
runtime dispatched exactly three, returned one ordered
`tool_call_limit_exceeded` result, and prepared the second provider request
with zero dispatchable tools. This is loop prevention, not a claim that every
workflow should use a limit of three. The author or operator must choose a
correct bound for the directive.

Compliant controls were unchanged in shape: one-query workloads remained at
two provider calls and one tool, and a three-query discovery workflow remained
at four provider calls and three tools. Their wall times moved in both
directions because provider time varied. A hard tool-call budget has no
latency effect on a zero-tool response.

## Prompt composition conclusions

Profiling records must report provider-ready request bytes and estimated tokens
for every call, including:

- directive template, system context, context positions, and request inputs;
- assistant, tool, and reasoning-replay message bytes;
- provider tool-schema count, bytes, and digest; and
- request-preparation duration.

In the captured first request, two tool schemas contributed roughly 7.9 KB of
the 9.2 KB body. Serialization itself took less than one millisecond. Caching
the local serialization can therefore save CPU but cannot remove provider
prompt ingestion unless the provider exposes and confirms a server-side prompt
cache hit.

Unchanged schemas and context are necessarily present in each stateless
provider request. RyeOS should avoid rebuilding them unnecessarily, but must
not confuse local byte reuse with provider-side token reuse. Later tool results
are the principal growth vector and need byte/token observability on every
round.

`limits.provider_request_body_bytes` is the exact serialized-body backstop for
that growth. It is intentionally a byte limit, not a guessed token limit. The
zero value remains unlimited, the effective value is daemon-sealed and parent/
operator capped, and refusal contains sizes only—never prompt or tool-result
content.

## Cache interpretation

A `launch_augmentation_cache_hit` identifies the exact augmentation, cache-key
digest, whether the caller waited for a concurrent fill, and that the child
execution was omitted. It does not mean that prompt construction, provider
inference, tool execution, or transcript persistence was skipped.

Signed provider usage may separately report cache-read, cache-miss, and
cache-write token dimensions. A declared read/miss partition is valid only when
its checked sum equals total input tokens. Cache misses are not cache writes,
and equal local request digests never fabricate a provider-side hit.

Safe caches remain content-addressed and authority-scoped. Concurrent identical
misses should use single-flight filling. Never share mutable results, secrets,
conversation state, or authority-dependent material across isolation domains.

Candidate caches must be justified by measured local cost:

- verified immutable item resolution and attestation;
- deterministic successful parse results;
- stable provider-ready tool-schema serialization; and
- compiled prompt fragments whose identity includes every content and authority
  dependency.

Do not cache model answers or tool results as a generic latency optimization.

## Kind-free extension boundaries

Profiling, inventory, execution policy, and runtime hard limits remain
data-driven extension surfaces:

- a signed runtime descriptor declares bounded child-observation envelopes;
- a signed inventoried-kind schema declares its admission capability template
  and required descriptor metadata;
- execution overrides are keyed by `items.<kind>.<bare_id>`; and
- a signed runtime descriptor declares opaque numeric limit dimensions and a
  stable inheritance contract.

Engine and executor code may validate, render, merge, clamp, and transport
these declarations, but must not interpret directive, provider, tool, graph,
prompt, cache, or application-specific field names. Tests in those crates must
use neutral fixture names for the same reason.

## Optimization order

Apply changes in descending expected impact:

1. Bound serial work with signed turns, tool calls, duration, tokens, and spend.
   Refused calls must remain ordered and auditable, and a resume must not regain
   budget.
2. Evaluate an explicit signed reasoning/model route on representative no-tool,
   one-tool, and multi-tool workloads. Compare quality and tool correctness, not
   only greetings. Absence must retain the provider default.
3. Reduce transcript growth at its source: compact tool results, avoid repeated
   metadata, and expose only the signed route's required tools. Keep this
   data-driven; the engine must not recognize a special item kind, provider,
   model, application, or keyword.
4. Capture provider cache metadata and usage when the provider offers an
   authenticated prompt-cache contract. Do not infer a hit from equal request
   digests.
5. Continue bounded progressive callback batching and SSE flush measurement.
   Preserve first event, text ordering, tool ordering, and terminal durability.
6. Consider managed runtime workers only if the residual RyeOS-owned first-call
   cost passes the worker design's pull-forward gate.

## Workers and chat

Workers can help chat throughput and cold-tail latency by retaining a verified
runtime process and, within a matching authority/transport partition, a warm
HTTP connection. They do not make a serial reasoning/tool loop parallel and do
not shorten provider inference.

The captured warm launch was already about 0.23 seconds, while first-call
DNS/connection establishment was under 0.1 seconds in the cited sample. That
does not meet the current worker pull-forward gate. Workers remain useful if a
larger cold/warm distribution later shows a repeatable material residual, but
they are not the next answer for multi-second first text or 40-80 second
multi-round requests.

See `knowledge:ryeos/future/content-addressed-managed-runtime-workers` for the
authority and isolation design.

## Benchmark discipline

For each workload, retain cold and warm samples and report session creation,
planning, admission, runtime-ready, all provider milestones, first visible
text, tool timings, model/tool-call counts, request bytes or estimated tokens
per provider call, cache outcome, total duration, and terminal status.

Use medians and tail percentiles once sample count permits. A one-off before and
after wall-clock change is not causal when provider timing changed. Mechanism
claims should instead be supported by invariant evidence such as work omitted,
number of provider calls avoided, dispatches bounded, request bytes removed, or
connections reused.

## Non-goals and safety constraints

- no canned greetings, keyword classifier, or fabricated progress;
- no exposure of hidden reasoning;
- no bypass of signatures, launch authority, vault isolation, accounting,
  tenant separation, audit logs, or result ordering;
- no application-specific or provider-specific branch in the engine;
- no unsafe cross-authority cache or worker reuse; and
- no downstream workaround for a RyeOS-owned cost.
