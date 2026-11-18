<!-- ryeos:signed:2026-11-18T01:04:43Z:49fb3ed2be105630b3e473b148129261b4a0304439924155a6b0d6807b3ead5f:BWKLf5lhruYulUFwbuupNXTDloRZ72Ulq6zXhmaNf91AU2Tty6Dr76R1RFLfP8hl5ncPCMIZDh1/uJcIWzjSDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
tags: [reference, execution, retry, timeout, config]
version: "1.0.0"
description: >
  Execution configuration — timeouts, retries, backoff, and
  the config resolution chain.
---

# Execution Configuration

The directive bundle provides runtime execution defaults that control
how LLM API calls are made — timeouts, retries, backoff, and more.

## Config File

`config:ryeos-runtime/execution` defines:

| Setting              | Default  | Description                           |
|----------------------|----------|---------------------------------------|
| `retries`            | 2        | Max retry attempts per API call       |
| `retry_status_codes` | 429, 500, 502, 503, 504, 529 | HTTP codes that trigger retry |
| `never_retry`        | 401, 403, 404 | HTTP codes that never retry    |
| `backoff_base_ms`    | 1000     | Exponential backoff base (ms)         |
| `backoff_max_ms`     | 60000    | Cap on any single backoff delay (ms); must not be below `backoff_base_ms` |
| `on_output_limit` | absent | Recovery when the provider ends a turn at its output limit; absent fails the thread |
| `unknown_outcome_retry` | absent | Retry policy after a failure that leaves the provider outcome unknown; see below |
| `timeout_seconds`    | 300      | Overall request timeout               |
| `max_provider_output_tokens_per_turn` | 32768 | Provider-native output-token ceiling sent through the provider's signed output-limit schema |
| `max_stream_output_bytes_per_turn` | 131072 | Exact local UTF-8 semantic-output byte backstop; `0` disables |
| `max_provider_stream_frame_bytes` | 1048576 | Maximum buffered logical SSE event size; `0` disables |
| `accounting.failure_policy` | auto | Fail closed for unavailable usage with a finite token/spend budget; otherwise warn |
| `accounting.budget_mode` | settled | Settle after each attempt; may overshoot a finite budget by one attempt |
| `tool_images` | see below | Ceilings on the images a run's tools return |
| `tool_preload`       | false    | Whether to preload tool definitions   |

## Retry Behavior

When an API call fails with a retryable status code (429, 500, 502, 503, 504, 529):
1. Wait `min(backoff_base_ms * 2^attempt, backoff_max_ms)` milliseconds
2. Retry the request
3. Repeat up to `retries` times (default: 2 → 3 total attempts)

Auth errors (401, 403) and not-found (404) are never retried.

Connection-establishment failures and explicitly configured retryable HTTP
statuses are always retried within `retries`. A timeout, non-connect send
failure, or mid-stream cut may have reached the provider; reusing the same
prompt bytes does not make a provider call idempotent. Such a failure is
retried only when all three hold:

1. The directive header declares `provider_contact: repeatable`, the author's
   statement that issuing the model call twice has no effect beyond its charge.
2. The selected provider route is a remote transport. An admitted local worker
   never gains repeated contact.
3. `unknown_outcome_retry` is present and its `on` list names the failure
   class (`timeout`, `send`, `mid_stream`).

```yaml
unknown_outcome_retry:
  max_retries: 2          # the first N retries of a turn may follow an unknown outcome
  on: [timeout, send, mid_stream]
```

`max_retries` must not exceed `retries`. Status retries and unknown-outcome
retries share the early attempts: attempt N may be retried after an unknown
outcome only while N is at most `max_retries`. Launch preparation seals the
resulting ceiling into the launch capsule and the daemon refuses any unknown-
outcome retry beyond it, so editing this file mid-run cannot widen it. Every
retry is charged the full reserved maximum for the abandoned attempt. The
removed `retry_on_timeout` and `retry_mid_stream` keys are refused.

### Output limit

A provider that ends a turn at its output limit (finish reason `length`) fails
the thread unless `on_output_limit` is set:

```yaml
on_output_limit:
  max_consecutive: 2
  message: "Your reply was cut at the output limit. Continue from where it stopped; put large content in pieces or by path."
```

The turn is then recorded with `finish: length`, keeping only its complete
parts (a tool call cut by the limit has unparsable arguments and is refused as
a malformed call). If the turn made no tool call, the runtime records `message`
as a harness input (`kind: output_limit`) and takes another turn. If it made
tool calls, they run and the next turn follows normally. After `max_consecutive`
turns in a row ending at the limit the thread fails as it would without the
policy; the count is derived from the record, so resume keeps it exact. The
turn limit still applies. `max_consecutive: 0` disables recovery.

`timeout_seconds` is a total request deadline. A stream still running past it is
cut and surfaces as a mid-stream failure, so size it for the longest legitimate
response.

The provider-native token ceiling and RyeOS-local byte backstop are independent.
RyeOS sends the token ceiling only at the signed provider schema's declared
request path. It never treats final provider usage metadata as proof that local
stream output crossed a limit. The local backstop counts exact UTF-8 bytes of
accepted visible text, emitted reasoning, and raw tool arguments before live
publication. It is intentionally reported as bytes, not as an estimated or
universal token count. Local cap failures are terminal for the turn rather than
retryable provider errors.

Every provider issue is preceded by an indexed
`provider_attempt_accounting` pending record and followed by a reported,
spend-only, unavailable, cancelled, or interrupted state. `settled` budget
mode preserves the after-attempt budget contract and is deliberately labeled
as capable of a one-attempt overshoot. `hard` is rejected during config
validation in this runtime build: RyeOS must not claim a hard budget until the
durable reservation/reconciliation backend is enabled. Reservation ceiling
fields are rejected in `settled` mode rather than silently ignored.

## Tool images

A tool returns images by answering with a JSON object whose only key is
`result_parts`, a list of `{"type": "text", "text": ...}` and
`{"type": "image", "media_type": ..., "data_b64": ...}` parts. Any other result
is an ordinary result. The runtime checks each image against its real header
(png, jpeg, gif and webp are read), the model's declared form and these
ceilings, stores the bytes as thread attachments and records only their hashes.
An image that fails a check is dropped and the result names a stable code:
`image_oversize`, `image_media_type`, `image_decode`, `image_limit` or
`image_input_unsupported`. A malformed envelope is an error result,
`tool_result_parts_invalid`.

```yaml
tool_images:
  max_bytes_per_image: 3145728     # keep within the node's thread_attachments limit
  max_images_per_result: 4
  max_bytes_per_result: 6291456    # image bytes in one result, before base64
  max_images_per_run: 256
  max_bytes_per_run: 50331648      # distinct image bytes a run may store
```

The per-run counters are rebuilt from the record on resume and continuation.
The node's `thread_attachments` policy still bounds every append, and a model's
own `image_input.accepts.max_bytes_per_image` can lower the per-image limit.

## Config Resolution Chain

Execution config is resolved via `config_resolve: deep_merge`:

1. **Base:** `config:ryeos-runtime/execution` (from directive bundle)
2. **Project override:** `.ai/config/ryeos-runtime/execution.yaml`

Deep merge means you can override individual settings without
repeating the entire config:

```yaml
# .ai/config/ryeos-runtime/execution.yaml
retries: 5
timeout_seconds: 600
```

This keeps `backoff_base_ms`, `never_retry`, etc. at their defaults.

## Tool-Level Config

Individual tools can also override execution params via their
`execution_params` field:

```yaml
execution_params:
  max_steps: 50
  max_concurrency: 5
```

Resolution: tool params > project config > user config > bundle defaults.

## The Core Execution Config

Separately, the core bundle's `config:execution/execution` controls
**subprocess** execution (tool timeouts, step limits, cancellation):

| Setting                  | Default  |
|--------------------------|----------|
| `timeout`                | 300s     |
| `max_steps`              | 100      |
| `max_concurrency`        | 10       |
| `cancellation_mode`      | graceful |
| `cancellation_grace_secs`| 5        |

The two cancellation keys are the stop policy for every managed launch:
directive and graph runtimes, tools, and workers. The engine resolves them
per item (defaults, then exact item overrides, then direct
`cancellation_*` params) and the daemon records the result in the launch's
metadata; `thread cancel`, daemon drain and recovery stop the process under
that recorded policy. A `native_async` declaration supplies its own policy
instead. The node caps the grace at 5 seconds whatever the config says.
The policy is node-local: a same-node continuation keeps the recorded policy,
while a cross-node worker handoff re-resolves it from the target node's config.

Both configs coexist — one for LLM API calls, one for subprocess
management.
