<!-- ryeos:signed:2026-05-21T11:11:49Z:06a10a23ee22eeff6c6e4663737aa44c7a914ee3ad1344bf3343232a7e1dcc07:Pwm93gOYTjzJ1QyhVGN3LO2Bt2l57dTBjlfFdVclsF380dOdu3B6wCr8DI/h0nZ9zDkmo7tixfE2QeHBnniRAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

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
| `tool_preload`       | false    | Whether to preload tool definitions   |
| `retry_on_timeout`   | true     | Whether timeouts trigger retries      |

## Retry Behavior

When an API call fails with a retryable status code (429, 500, 502, 503):
1. Wait `backoff_base_ms * 2^attempt` milliseconds
2. Retry the request
3. Repeat up to `retries` times (default: 2 → 3 total attempts)

Auth errors (401, 403) and not-found (404) are never retried.

Timeouts are retried by default (`retry_on_timeout: true`).

## Config Resolution Chain

Execution config is resolved via `config_resolve: deep_merge`:

1. **Base:** `config:ryeos-runtime/execution` (from standard bundle)
2. **User override:** `~/.ryeos/.ai/config/ryeos-runtime/execution.yaml`
3. **Project override:** `.ai/config/ryeos-runtime/execution.yaml`

Deep merge means you can override individual settings without
repeating the entire config:

```yaml
# ~/.ryeos/.ai/config/ryeos-runtime/execution.yaml
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
