<!-- ryeos:signed:2026-07-22T08:09:36Z:787b6beb4add88d058b273ecbe6d79a23909a3ce3200803126bc627595ec21b1:sjcMmsCQkap5sgbdKjPzzNr/pGFjxPsP6paCuTb3ijw7F4E+uOC9AHWltXXOdHqGbvYyhyRCi+2gCn7Rv3tfDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
tags: [reference, execution, retry, timeout, config]
version: "1.0.0"
description: >
  Execution configuration — timeouts, retries, backoff, and
  the config resolution chain.
---

# Execution Configuration

The standard bundle provides runtime execution defaults that control
how LLM API calls are made — timeouts, retries, backoff, and more.

## Config File

`config:ryeos-runtime/execution` defines:

| Setting              | Default  | Description                           |
|----------------------|----------|---------------------------------------|
| `retries`            | 2        | Max retry attempts per API call       |
| `retry_status_codes` | 429, 500, 502, 503 | HTTP codes that trigger retry |
| `never_retry`        | 401, 403, 404 | HTTP codes that never retry    |
| `backoff_base_ms`    | 1000     | Exponential backoff base (ms)         |
| `timeout_seconds`    | 300      | Overall request timeout               |
| `max_provider_output_tokens_per_turn` | 32768 | Provider-native output-token ceiling sent through the provider's signed output-limit schema |
| `max_stream_output_bytes_per_turn` | 131072 | Exact local UTF-8 semantic-output byte backstop; `0` disables |
| `max_provider_stream_frame_bytes` | 1048576 | Maximum buffered logical SSE event size; `0` disables |
| `accounting.failure_policy` | auto | Fail closed for unavailable usage with a finite token/spend budget; otherwise warn |
| `accounting.budget_mode` | settled | Settle after each attempt; may overshoot a finite budget by one attempt |
| `tool_preload`       | false    | Whether to preload tool definitions   |
| `retry_on_timeout`   | true     | Whether timeouts trigger retries      |

## Retry Behavior

When an API call fails with a retryable status code (429, 500, 502, 503):
1. Wait `backoff_base_ms * 2^attempt` milliseconds
2. Retry the request
3. Repeat up to `retries` times (default: 2 → 3 total attempts)

Auth errors (401, 403) and not-found (404) are never retried.

Timeouts are retried by default (`retry_on_timeout: true`).

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

## Config Resolution Chain

Execution config is resolved via `config_resolve: deep_merge`:

1. **Base:** `config:ryeos-runtime/execution` (from standard bundle)
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

Both configs coexist — one for LLM API calls, one for subprocess
management.
