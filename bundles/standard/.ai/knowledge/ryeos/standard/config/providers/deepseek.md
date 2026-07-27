---
category: ryeos/standard/config/providers
tags: [provider, deepseek, v4, models]
version: "1.0.0"
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
