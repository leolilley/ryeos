<!-- ryeos:signed:2026-08-02T02:55:18Z:c18ec81fc208e62e6ba7e5d3a54c4b70568fbaa9e2f8c4ead05fd27056903b58:fnbHdfT60UlQjF0OV1L7SOzE1R0Kobcv4+8KSxFcQkSQAAto9TdOFLS/SOAA52htxEmp9YFdanvp9eBmYNu+DA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/development
name: chat-latency-investigation
title: Chat Latency Investigation Runbook
description: Correlation rules, event semantics, prompt accounting, and decision gates for streamed directive latency investigations
entry_type: runbook
version: "1.0.0"
```

# Chat Latency Investigation Runbook

Use this runbook when a downstream chat application reports slow first text or
long tool loops. Do not infer provider reasoning time from the interval between
gateway `stream_started` and visible text. Correlate the downstream request,
RyeOS launch, directive process, every provider call, and every tool call by
`launch_id`, `thread_id`, turn, and attempt.

## Profiling build

Detailed measurements belong to a release-equivalent profiling build, not an
ordinary release or Cargo's unoptimized development profile. Build the same
release binaries and enable the disabled-by-default feature through either:

```sh
./scripts/populate-bundles.sh \
  --key <publisher-key> \
  --owner <publisher> \
  --bundle-set <set> \
  --latency-profiling \
  --all
```

or container build argument `RYEOS_LATENCY_PROFILING=1`. Never enable the mode
on production while investigating a downstream workload. Every profiled
directive process emits `latency_profiling_enabled`; absence of that record
means detailed results must not be attributed to the profiling build.

The feature adds measurements and a small number of advisory callback events,
so it has observer cost. Compare ordinary and profiling artifacts on a bounded
local fixture before treating very small differences as application latency.
Profiling fixtures remain generic: workload-specific prompts and data belong in
the external benchmark driver, never in RyeOS code or tests.

## Event semantics

- Gateway `execution_planning` means durable launch planning has begun.
- Gateway `stream_started` means the launch handoff succeeded and the RyeOS
  subscription is ready. It does not mean a provider request was submitted and
  it does not prove that provider output exists.
- Profiling runtime `provider_response_headers` is emitted after successful
  provider HTTP headers arrive.
- Profiling runtime `provider_stream_started` is emitted after the first
  complete provider stream event is parsed.
- Profiling runtime `provider_reasoning_started` is emitted on the first
  reasoning delta. It contains no chain-of-thought content.
- The first non-whitespace `cognition_out.delta` is the child-side proxy for
  first visible assistant text. Downstream delivery must be measured in the
  application clock domain.

Provider lifecycle events are ephemeral. Durable audit, usage accounting,
tool-call ordering, and transcript events retain their existing storage rules.

## Required timeline

For each sample record:

1. downstream request and session creation;
2. planning start and commit;
3. scheduler admission and launch handoff;
4. gateway `stream_started`;
5. directive process entry, envelope parse, bootstrap, and request preparation;
6. provider request submission, headers, first event, first reasoning, and first
   visible text;
7. each tool start/result and the next provider request;
8. final provider completion, accounting settlement, transcript persistence,
   and downstream terminal delivery.

Daemon and directive-process timings use different monotonic clocks. Compare
durations inside a clock domain; join domains at their correlated wall-clock
log timestamps rather than subtracting unrelated monotonic offsets.

## Prompt accounting

In profiling builds, `directive_prompt_source_composition` records the
directive template, rendered system/before/after context sizes, tool count, and
serialized size of every named request input. `directive_context_composition`
records each signed context item and its composer token estimate.
`directive_provider_request_profile` records the exact provider-ready body
size, source-message role sizes, converted-message size, tool-schema size and
digest, reasoning replay bytes, and an explicitly heuristic
`ceil(body_bytes / 4)` token estimate. `directive_provider_call_timing` retains
the provider/network timeline and joins by provider call ID.

Compare provider-call records by turn:

- a stable tool-schema digest with repeated schema bytes proves definitions are
  serialized into each provider request;
- message/body growth after a tool result shows the marginal transcript cost;
- provider-reported usage is authoritative when available; byte-based token
  estimates are diagnostic only;
- never log prompt text, tool-result text, credentials, or reasoning content to
  obtain these measurements.

## Cache audit

Name the cache and exact authority boundary. Record hit, miss, stale,
single-flight wait, bypass, lookup time, and the work skipped. A cache hit is not
evidence that all launch work was skipped.

Mutable live projects require content/precedence revalidation. Cache keys must
bind the relevant project generation, trust/registry generation, sealed
capability closure, parser generation, and configuration identity. Do not share
provider-ready bodies, prompts, transcripts, secret-bound material, or mutable
tool descriptors across authorities.

Do not infer inventory candidates from capability string syntax in the
executor. Inventory kinds and discovery semantics are schema-declared; the
engine remains kind-free at this boundary. A future optimization must either
be schema-declared or cache an opaque, exact parser/projection result under all
content, precedence, parser, trust, and authority generations that can affect
it. Profiling evidence must identify the expensive generic phase before such a
cache is designed.

## Decision gates

Separate application overhead, RyeOS launch/runtime overhead, provider network
time, provider inference, tools, and repeated provider rounds. Optimize the
largest measured component.

Do not pull forward managed runtime workers from total chat latency alone. Use
the gates in
`knowledge:ryeos/future/content-addressed-managed-runtime-workers`. In
particular, subsecond warm launch handoff, sub-500ms child-to-provider
submission, and small connection setup costs point away from workers.

Reasoning controls are a semantic policy. Benchmark the same provider/model and
provider-ready request with reasoning enabled and disabled, then expose the
choice through signed configuration. Do not use keywords, greeting shortcuts,
or an automatic complexity guess to change reasoning mode.

## Benchmark discipline

Use a non-production environment. Take a small cold/warm matrix covering simple
chat, no-tool data summary, one-tool analysis, bounded aggregation, and bounded
discovery/trend workflows. Preserve provider/model identity and request shape.
Report every sample, including failures; do not publish only medians or only
successful runs. Keep paid repetitions low while taking enough alternating
samples to expose provider variance.
