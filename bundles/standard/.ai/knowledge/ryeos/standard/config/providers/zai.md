<!-- ryeos:signed:2026-08-11T02:28:37Z:a4ae500d2bd246b1301f647156322f436b877c204fdcd677a11a7f5a351ccb6d:RzeA/rCQpxvzr9Zjg6MxAZt6Qtiq64ztQUx3untFjqFPqCU0wRWe2tESDz3cgdeoujCYfNCUq3zXc4JPvoJZAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config/providers
tags: [provider, zai, glm, models]
version: "1.0.0"
description: Direct Z.AI provider config reference.
---

# Provider Config: zai

The `zai` provider calls Z.AI's general API directly through the
OpenAI-compatible Chat Completions protocol. It uses:

- `https://api.z.ai/api/paas/v4` as the authoritative base URL;
- `ZAI_API_KEY` as a Bearer credential;
- `/chat/completions` with streamed reasoning, text, and function calls;
- `max_tokens` as the provider-native output limit;
- the final SSE frame's `usage.prompt_tokens` and
  `usage.completion_tokens` for required accounting.

Z.AI sends token usage in the last stream frame without requiring
`stream_options.include_usage`, so the signed request body does not send that
OpenAI-specific option. The provider enables `tool_stream` for the advertised
GLM models so function arguments arrive incrementally and remain compatible
with RyeOS's `delta_merge` stream parser.

The bundled price table uses Z.AI's direct API prices in USD per million
tokens. RyeOS's two-rate estimate currently prices all reported prompt tokens
at the list input rate; it does not apply Z.AI's separate cached-input
discount. Unknown model names have no fallback price, so they remain unpriced
rather than being silently treated as free.

`glm-4.7-flash` is the explicitly priced zero-cost option. Paid models,
including `glm-5.2`, require general API balance or an applicable API resource
package; a Coding Plan subscription does not fund calls to the general API.
The signed `free-fast-no-thinking` profile disables provider reasoning only for
that zero-priced model, matching the fast request shape used for latency
measurement. Other advertised models retain Z.AI's normal reasoning behavior.

These per-model prices currently provide estimates and settlement
presentation; they are not a hard spend certificate. The provider intentionally
has no `spend_authority`, so a launch requiring hard accounting fails closed for
every Z.AI model. Exact zero pricing for `glm-4.7-flash` must not be confused
with the provider-wide `pricing.explicitly_free` authority used by wholly free
providers.

## Coding Plan is a separate product

This provider deliberately does not use
`https://api.z.ai/api/coding/paas/v4`. Z.AI restricts Coding Plan subscription
quota to its documented supported tools and product environments, and RyeOS is
not one of them. A Coding Plan subscription therefore does not authorize this
provider; use a normal Z.AI API key with API balance.

There is no Coding Plan alias or fallback. Adding that endpoint would require
Z.AI to authorize RyeOS as a supported integration and would be represented by
a distinct signed provider contract.

## Explicit selection

A directive can select the provider with a coherent model tuple:

```yaml
model:
  provider: zai
  name: glm-5.2
  context_window: 1000000
```

The standard tier routing table is unchanged. Selecting `zai` is explicit
until an operator intentionally changes a signed routing tier.
