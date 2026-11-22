<!-- ryeos:signed:2026-11-17T17:48:41Z:c6cae8f1534e1981b119d572af6afec10900b163b2fba955af3587f732edc590:L29CBHqyLbi+JyyliH8dmQtAgs4/dboVNVs0FZbFjBlcPepVZ0Fh6/zdVTDwLK8D7BtwYtXhzvEl4N3aeaboDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config/providers
tags: [provider, deepseek, v4, models]
version: "1.2.0"
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

The directive bundle exposes `deepseek-v4-pro` and `deepseek-v4-flash`. Both
have a 1,000,000-token context window, support thinking and non-thinking modes,
and support tool calls. Thinking mode is provider-enabled by default. RyeOS
records streamed `reasoning_content` as a `reasoning` part of the assistant turn,
in stream order beside its tool calls, and sends it back on assistant messages
as `reasoning_content`, so it is supplied on the following tool-result turn as
required by DeepSeek. Both the capture and the replay are declared by the signed
entry under `schemas.reasoning`.

The legacy `deepseek-chat` and `deepseek-reasoner` aliases are not advertised:
DeepSeek retired them on 24 July 2026.

## Accounting

The provider requires the final stream usage snapshot. Reported
`completion_tokens` include reasoning tokens, with the reasoning subset read
from `completion_tokens_details.reasoning_tokens`. The signed usage schema also
reads `prompt_cache_hit_tokens` and `prompt_cache_miss_tokens` and declares that
they partition `prompt_tokens`; a missing field, malformed count, overflow, or
partition mismatch invalidates token settlement instead of inventing a cache
hit.

Signed spend profiles reserve a conservative upper bound and settle the actual
cache-hit/cache-miss partition at DeepSeek's USD rates:

| Model | Cache hit / 1M | Cache miss / 1M | Output / 1M |
|---|---:|---:|---:|
| `deepseek-v4-pro` | $0.003625 | $0.435 | $0.87 |
| `deepseek-v4-flash` | $0.0028 | $0.14 | $0.28 |

Provider-reported cache counts remain usage/accounting facts. Equal request
digests do not synthesize cache hits, and RyeOS does not share provider cache
state across authorities.

## Explicit selection

```yaml
model:
  provider: deepseek
  name: deepseek-v4-pro
  context_window: 1000000
```

The directive bundle's routing table is unchanged. Selecting `deepseek` remains an
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
