<!-- ryeos:signed:2026-08-02T07:16:40Z:3ece61307e2f48e03231c8149818a8595db5cd3ba293b81ec0c62ea0d8f09456:DrXqmqrQtKcpHl3PbkN6U1QWiYREOlxCR2q8TJ+8lTXgEooQvb12Yed1OjezQSx8S7wP8OAQU7LXYtvz7qkaCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config/providers
tags: [provider, deepseek, v4, models]
version: "1.1.0"
description: Direct DeepSeek provider config reference.
---

# Provider Config: deepseek

The `deepseek` provider calls DeepSeek's official API directly through its
OpenAI-compatible Chat Completions protocol. It uses:

- `https://api.deepseek.com` as the authoritative base URL;
- `DEEPSEEK_API_KEY` as a Bearer credential;
- `/chat/completions` with streamed reasoning, text, function calls, and a
  final usage frame;
- `max_tokens` as the provider-native output limit.

The standard bundle exposes `deepseek-v4-pro` and `deepseek-v4-flash`. Both
have a 1,000,000-token context window, support thinking and non-thinking modes,
and support tool calls. Thinking mode is provider-enabled by default. RyeOS
preserves streamed `reasoning_content` on assistant tool-call messages so it
can be supplied on the following tool-result turn as required by DeepSeek.

The legacy `deepseek-chat` and `deepseek-reasoner` aliases are not advertised:
DeepSeek retired them on 24 July 2026.

## Accounting

The provider requires the final stream usage snapshot. Reported
`completion_tokens` include reasoning tokens, with the reasoning subset read
from `completion_tokens_details.reasoning_tokens`.

Signed spend profiles use DeepSeek's USD cache-miss input prices as the
conservative upper bound:

| Model | Input / 1M | Output / 1M |
|---|---:|---:|
| `deepseek-v4-pro` | $0.435 | $0.87 |
| `deepseek-v4-flash` | $0.14 | $0.28 |

Cache-hit input is cheaper, but RyeOS does not rely on that discount for hard
spend admission.

## Explicit selection

```yaml
model:
  provider: deepseek
  name: deepseek-v4-pro
  context_window: 1000000
```

The standard routing table is unchanged. Selecting `deepseek` remains an
explicit project or directive routing decision.

## Explicit reasoning policy

The signed provider descriptor maps the provider-neutral policy to DeepSeek's
`thinking.type` and `reasoning_effort` fields. To retain provider defaults,
omit `reasoning` entirely. To request the lower-latency non-thinking mode:

```yaml
model:
  provider: deepseek
  name: deepseek-v4-pro
  context_window: 1000000
  reasoning:
    mode: disabled
```

To keep thinking enabled with an explicitly supported effort:

```yaml
model:
  provider: deepseek
  name: deepseek-v4-pro
  context_window: 1000000
  reasoning:
    mode: enabled
    effort: high
```

This is a signed model-policy choice, not an automatic prompt classifier or a
global DeepSeek default. RyeOS does not expose streamed reasoning content as
visible assistant text.
