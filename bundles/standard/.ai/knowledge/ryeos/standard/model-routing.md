<!-- ryeos:signed:2026-05-22T04:30:10Z:f2b10ff2f9eff77034d868c6a6c44d4d7410ea4699be2b4c4a315f2634c303e1:Mf/9TFSVG/vB8DHr0Fda9BQ7z7boiNgp4ou6XvKRk2NrrrU3BU8aZrL2QWfaXBv+4CQwrdOpWi9RM3AUiDNtBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
tags: [models, providers, routing, tiers, llm]
version: "1.1.0"
description: >
  How model routing works — the tier system, the routing table,
  and the active provider configs (Anthropic, OpenAI, Zen).
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

## Provider configs

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

Legacy provider tool descriptors were removed: the directive runtime now
uses the runtime-level provider configs directly. Adding a provider means
adding a signed config under `config/ryeos-runtime/model-providers/` and,
if it should be selected by tier, pointing `model_routing.yaml` at that
provider.

## API Keys

Each provider reads its API key from an environment variable:

| Provider    | Env Variable          |
|-------------|-----------------------|
| Zen         | `ZEN_API_KEY`         |
| Anthropic   | `ANTHROPIC_API_KEY`   |
| OpenAI      | `OPENAI_API_KEY`      |

Set these in the daemon's environment or in `.ai/config/` secrets.
