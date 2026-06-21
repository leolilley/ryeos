<!-- ryeos:signed:2026-06-21T03:35:57Z:f4c9a5aa1581d47610ab08e210ac5554bfbcf0d885cf759ee43b2aa183669762:jEWYn6SHwWakQ56hdgr2crWO48xy1SbCoMxQTDlFTs1Ofs/mmCEL69Uo/QDa/NIrqz/MLfTN7o7qbEen9it5Dw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
tags: [models, providers, routing, tiers, llm]
version: "1.2.0"
description: >
  How model routing works — the tier system, the routing table,
  and the active provider configs (Anthropic, OpenAI, Zen, local-openai),
  including offline/local routing for internet-disabled eval.
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

### Local / offline (`config:ryeos-runtime/model-providers/local-openai`)
An OpenAI-compatible server running on `localhost` (vLLM, llama.cpp,
text-generation-webui, etc.), for environments where the internet is
disabled — e.g. competition/eval mode. It uses the same
`chat_completions` family and `delta_merge` streaming as OpenAI, but:

- `base_url` points at `http://127.0.0.1:8000/v1`,
- `auth: {}` — **no credential header is sent**, so no API key env var is
  required and the directive adapter will not refuse the call,
- `pricing` is zero (local weights cost nothing), so cost accounting still
  records token counts without inventing a dollar figure.

`stream_options.include_usage` is requested so token usage is reported in
the final SSE chunk. If a particular server rejects that option, add a
sibling `local-openai-basic` config without `stream_options`; generation
still works, but usage/cost may be reported as zero.

Prior provider tool descriptors were removed: the directive runtime now
uses the runtime-level provider configs directly. Adding a provider means
adding a signed config under `config/ryeos-runtime/model-providers/` and,
if it should be selected by tier, pointing `model_routing.yaml` at that
provider.

## Offline eval: routing the same directives to a local model

The point of tier-based routing is that a directive says `model: {tier:
high}` (or relies on the default tier) and **never names a provider**. To
run those exact directives offline, override only the routing table at the
project level — no directive item changes:

```yaml
# config:ryeos-runtime/model_routing  (project-level override)
category: "ryeos-runtime"
tiers:
  fast:         {provider: local-openai, model: Qwen/Qwen2.5-7B-Instruct,  context_window: 32768}
  general:      {provider: local-openai, model: Qwen/Qwen2.5-7B-Instruct,  context_window: 32768}
  high:         {provider: local-openai, model: Qwen/Qwen2.5-14B-Instruct, context_window: 32768}
  orchestrator: {provider: local-openai, model: Qwen/Qwen2.5-14B-Instruct, context_window: 32768}
```

Dev (hosted, via Zen) → eval (local) becomes a routing-config swap, not a
code or directive change. Note: provider configs are trust-enforced, so a
project-root model-routing override that points at a provider is only
honored under the daemon's project-config policy (e.g.
`RYEOS_ALLOW_PROJECT_PROVIDER_CONFIG=1`); otherwise sign the override into
the active bundle.

## API Keys

Each provider reads its API key from an environment variable:

| Provider    | Env Variable          |
|-------------|-----------------------|
| Zen         | `ZEN_API_KEY`         |
| Anthropic   | `ANTHROPIC_API_KEY`   |
| OpenAI      | `OPENAI_API_KEY`      |

Set these in the daemon's environment or in `.ai/config/` secrets.
