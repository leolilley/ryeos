---
category: ryeos/standard
tags: [models, providers, routing, tiers, llm]
version: "1.0.0"
description: >
  How model routing works — the tier system, the routing table,
  and the four providers (Anthropic, OpenAI, OpenRouter, Zen).
---

# Model Routing

Rye OS uses a **tier-based routing system** to abstract model selection.
Directives request a capability tier; the routing table maps it to a
concrete model and provider.

## Tiers

| Tier            | Purpose                           | Default Model (via Zen) |
|-----------------|-----------------------------------|-------------------------|
| `fast`          | Quick responses, low cost         | claude-haiku-4-5        |
| `general`       | General-purpose (default)         | claude-haiku-4-5        |
| `high`          | Complex reasoning                 | claude-sonnet-4-6       |
| `orchestrator`  | Multi-step orchestration          | claude-sonnet-4-6       |
| `max`           | Maximum capability                | claude-opus-4-7         |
| `code`          | Code generation/editing           | gpt-5.4                 |
| `code_max`      | Complex code tasks                | gpt-5.5-pro             |
| `cheap`         | Cost-optimized                    | gpt-5.4-nano            |
| `free`          | No-cost (rate-limited)            | minimax-m2.5-free       |
| `vision`        | Image understanding               | gemini-3.1-pro          |
| `vision_fast`   | Fast image processing             | gemini-3-flash          |
| `kimi`          | Kimi models                       | kimi-k2.6               |
| `glm`           | GLM models                        | glm-5.1                 |
| `qwen`          | Qwen models                       | qwen-3.6-plus           |

## The Routing Table

Defined in `config:ryeos-runtime/model_routing`. All tiers currently
route through the **Zen** provider (`opencode.ai/zen`). The routing
table specifies:

```yaml
tier_name:
  provider: zen
  model: claude-haiku-4-5
  context_window: 200000
```

## Four Providers

### Zen (`config:ryeos-runtime/model-providers/zen`)
The primary gateway. Routes to multiple model families through
`opencode.ai/zen`:
- **Anthropic Claude** (via Zen's Anthropic proxy)
- **OpenAI GPT** (via Zen's OpenAI-compatible endpoint)
- **Google Gemini** (via Zen's Google proxy)
- **Open-weights** (GLM, Kimi, Qwen, MiniMax, etc.)

Zen uses **profiles** — sub-configurations that match on model name
patterns and override the base config with provider-specific API
formats:
- `anthropic` profile for `claude-*` models
- `openai_compat` profile for `gpt-*`, `minimax-*`, `glm-*`, etc.
- `gemini` profile for `gemini-*` models

### Anthropic (`config:ryeos-runtime/model-providers/anthropic`)
Direct Anthropic Messages API access. Uses `x-api-key` auth,
`anthropic-version: 2023-06-01`, blocks-array message format.

### OpenAI (`config:ryeos-runtime/model-providers/openai`)
Direct OpenAI Chat Completions access. Uses `Bearer` token auth,
standard `chat/completions` endpoint.

### OpenRouter (`config:ryeos-runtime/model-providers/openrouter`)
Multi-model gateway via OpenRouter. Uses OpenAI-compatible format
with `HTTP-Referer` and `X-Title` headers.

## Agent Adapters

In addition to the runtime-level provider configs, the standard bundle
includes **agent adapter tools** that are full HTTP client specifications:

- `tool:ryeos/agent/providers/anthropic` — complete Anthropic client
- `tool:ryeos/agent/providers/openai` — complete OpenAI client
- `tool:ryeos/agent/providers/zen` — unified gateway client with profiles

These adapters include:
- Request building schemas (how to construct API calls)
- Response parsing schemas (how to read API responses)
- Streaming schemas (how to handle SSE deltas)
- Tool-use schemas (how to present/call tools)
- Pricing per model

The adapters are used by the directive runtime to communicate with
LLM providers during the prompt + tool loop.

## API Keys

Each provider reads its API key from an environment variable:

| Provider    | Env Variable          |
|-------------|-----------------------|
| Zen         | `ZEN_API_KEY`         |
| Anthropic   | `ANTHROPIC_API_KEY`   |
| OpenAI      | `OPENAI_API_KEY`      |
| OpenRouter  | `OPENROUTER_API_KEY`  |

Set these in the daemon's environment or in `.ai/config/` secrets.
