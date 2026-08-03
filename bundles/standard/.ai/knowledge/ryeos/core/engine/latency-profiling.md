<!-- ryeos:signed:2026-08-03T06:49:18Z:91c1d8e436b9bc4fb08434e39c93eb337b39e65a5f86ac3d3b0cb5f275bf4bff:2HSsAOuP0tnYTRzC0XH3EYqGxeUZfWqXh1J9buyrlAr/WkOPUhbEmamf39QKEAIv/rAubLnxsOEt5sPtIjdOAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/engine
tags: [latency, profiling, observability, streaming, providers, caching]
version: "1.1.0"
description: >
  How to build, measure, and interpret RyeOS latency without confusing launch,
  provider, reasoning, tool, and downstream delivery time.
---

# Latency Profiling

RyeOS latency must be measured as a sequence of independently attributable
stages. A single end-to-end number cannot distinguish node launch work from
provider variance, hidden reasoning, tool round trips, or downstream delivery.

The profiling build is opt-in and release-equivalent. It adds measurements and
content-free request composition summaries; it does not weaken launch
authority, vault isolation, usage accounting, audit persistence, or runtime
isolation.

## Build the complete profiling path

Build both the daemon and the directive runtime with the feature. Profiling
only the daemon cannot expose provider-side child timings, while profiling only
the runtime omits the daemon launch path.

```bash
cargo build --release \
  -p ryeosd -p ryeos-directive-runtime \
  --features ryeosd/latency-profiling,ryeos-directive-runtime/latency-profiling
```

For a locally populated dev bundle set, use the supported publisher workflow:

```bash
scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev \
  --bundle-set standard \
  --latency-profiling
scripts/pkg/install-local-direct.sh --trust-source-publishers
```

Do not ship profiling binaries as an accidental release artifact. A runtime
emits `latency_profiling_enabled`, and the daemon captures it as
`runtime_child_observation_record`, so a benchmark can prove which build ran.

## Event semantics

These milestones are deliberately distinct:

| Milestone | Meaning |
|---|---|
| `execution_planning` | The gateway has accepted the request and launch planning is in progress. |
| `stream_started` | Launch handoff is ready and the gateway can attach the thread stream. It says nothing about provider HTTP progress or visible model output. |
| `observation` with `kind: directive.provider_response_headers` | A profiling directive runtime received successful provider HTTP response headers. |
| `observation` with `kind: directive.provider_stream_started` | A profiling directive runtime parsed the first complete provider stream event for one model call. It may be metadata, reasoning, tool activity, or text. |
| `observation` with `kind: directive.provider_reasoning_started` | The runtime observed a reasoning delta. It contains no reasoning text and does not imply that reasoning is visible to the user. |
| first non-whitespace text | The runtime has had the daemon acknowledge publication of a visible text delta. Actual client receipt is measured by the client in its own clock domain. |

Never report `stream_started` as provider time-to-first-byte or time-to-first-
token. Never expose hidden reasoning merely to make the interface appear busy.

## Timing records

The daemon writes structured events to
`<app-root>/.ai/state/trace-events.ndjson`. Use identifiers to join records;
never subtract raw offsets from different clock domains.

Captured child records use one generic `runtime_child_observation_record` log
shape. The selected signed runtime descriptor declares each accepted child
event name, schema version, clock domain, record count, and byte ceiling. The
engine and executor validate only that envelope and treat the remaining JSON as
opaque runtime-owned data. Directive/provider field meanings stay in the
directive runtime and this profiling documentation.

- `launch_stage_timings` uses the daemon monotonic clock and records launch
  stages, milestones, the accounted critical path, and unattributed time.
- `directive_runtime_stage_timing` uses the directive-process monotonic clock
  for envelope parsing, process attachment, bootstrap, the first provider
  request, headers, provider event, reasoning event, and published visible
  text.
- `directive_provider_call_timing` records every bounded provider call by turn
  and attempt, including request submission, headers, first event, DNS,
  aggregate connection establishment, progressive callback batches, final
  provider-reported input/output/reasoning and cache-token usage, usage
  validity/source, and completion. The usage fields contain counts only; they
  never contain prompt, reasoning, or response content.
- `directive_provider_request_profile` reports serialized request bytes, a
  documented token estimate, message-role byte contributions, reasoning replay,
  and tool-schema bytes and digest. It does not record prompt content.
- `directive_context_composition` and
  `directive_prompt_source_composition` attribute context and input size before
  the provider request.
- `inventory_kind_build_timing` separates enumeration, resolution, file reads,
  parser execution, and projection.
- Cache metrics report hit, miss, bypass, single-flight, and eviction outcomes
  for their named cache. `parser_result_cache` additionally reports source
  bytes, retained entry bytes, and single-flight wait time without logging
  content or cache keys.

Connection establishment is the aggregate observed connector future and may
include DNS, TCP, proxy negotiation, and TLS. RyeOS currently does not claim an
exact TCP/TLS split when the transport cannot provide it.

## Benchmark method

Define workloads before running them and keep their inputs stable. Include at
least a no-tool request, a single-tool request, and a bounded multi-round
request. For every workload:

1. Start with a fresh daemon for the cold sample.
2. Run warm samples on the same daemon and engine generation.
3. Record session creation, planning, admission, launch handoff, every provider
   call, first provider event, first visible text, every tool interval, terminal
   status, and total duration.
4. Record provider ID and model, request bytes and estimated tokens per call,
   model-call count, tool-call count, and cache outcomes.
5. Use several warm samples when provider time dominates. Report the individual
   values or median and range; do not turn one provider outlier into a RyeOS
   constant.
6. Compare a direct-provider control using the same model, reasoning policy,
   prompt material, tools, region, and connection-reuse conditions. State every
   mismatch that remains.

Keep daemon, runtime, and client clock domains separate. Durations inside one
record are comparable; cross-process milestones are correlated by request,
invocation, and thread identifiers.

## Reading the latency budget

Classify the measured duration into:

- downstream client and network delivery;
- gateway planning, admission, and launch work;
- runtime bootstrap and request preparation;
- provider network time to headers and first event;
- provider reasoning and generation;
- tool execution;
- additional model/tool rounds;
- persistence and callback publication.

Prioritize repeated serial work on the critical path. Safe caches must bind all
inputs that can affect the output, default to disabled for extension points,
retain successful results only, remain bounded, and single-flight identical
misses. A cache hit must never bypass trust verification, authority admission,
tenant isolation, accounting, or audit logging.

## When workers help

A persistent worker helps only when measurements show that repeated process
startup or per-process initialization is a material part of the critical path.
It cannot remove provider reasoning, provider network variance, sequential
model/tool rounds, or oversized prompts.

Before introducing a worker, compare its measured saving with the added
lifecycle, crash recovery, version rollover, secret scoping, cancellation,
resource accounting, and tenant-isolation requirements. Chat workloads can use
workers, but persistence alone does not make first visible text fast. Prefer the
smallest change that removes the measured bottleneck while retaining the same
security and ordering guarantees.

## Reporting

A latency report should include the exact build and revision, environment,
workload, cold/warm state, sample count, event definitions, raw timings,
provider/model identity, request sizes, call counts, cache outcomes, and
terminal status. Separate observed evidence from inference and state explicitly
when a target is blocked by provider behavior rather than RyeOS overhead.
