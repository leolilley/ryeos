<!-- ryeos:signed:2026-07-27T01:32:33Z:73f653fd6e24d678da127b20482b4e111a3983451b1780c1581debdd86cea5d9:lkbzCwsvUl55oDL6QK3fYHBhXX1HtHP+Q7b3QDdtc9upmMF5s+mHL/1YHePTwmHX03wSPYTkrYLLAAEd+oRkBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
