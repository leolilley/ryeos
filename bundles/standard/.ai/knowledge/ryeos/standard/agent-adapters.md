<!-- ryeos:signed:2026-05-23T09:45:40Z:f2e07a584fe8411a301b1c291534e8865047ae791eeb3581e3a714df43a9be57:M0z8uYktGnsPqgZbLvoG5FqMfX3faD7EhenC4DQxh/M7djGynU7ylLo4Mjgm46GozjtjcdosudpMKY0DxMYCDw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->

---
tags: [reference, providers, zen, anthropic, openai, adapter]
version: "2.0.0"
description: >
  How LLM providers work — the directive runtime reads provider configs
  at launch time to build HTTP requests, parse responses, and handle
  streaming for each model family.
---

# LLM Provider Adapters

The directive runtime interacts with LLM providers directly using
runtime-level provider configs. Each provider config specifies:

- How to build requests (URL, headers, body template)
- How to parse responses (content extraction, tool call detection)
- How to handle streaming (SSE event types, delta merging)
- How to format tool results (wrapping for re-submission)
- Pricing (per-model cost per million tokens)

## Provider Configs

Provider configs are signed YAML files resolved at runtime launch and
frozen into a `ResolvedProviderSnapshot`. This avoids a time-of-check /
time-of-use split between daemon preflight and runtime HTTP calls.

## Anthropic Provider

**Config:** `config:ryeos-runtime/model-providers/anthropic`

### Tier Mapping
| Tier     | Model                       |
|----------|-----------------------------|
| fast     | claude-haiku-4-5-20251001   |
| general  | claude-haiku-4-5-20251001   |
| high     | claude-sonnet-4-5-20250929  |
| max      | claude-opus-4-6             |

### Message Format
- `blocks_array` text placement
- System message sent as body field (`system`)
- Tool results wrapped in `content_blocks` with `tool_result` blocks
- Tool calls use `inline_blocks` with `tool_use` blocks

### Streaming
- Mode: `event_typed` (SSE event types: `message_start`,
  `content_block_start`, `content_block_delta`, `message_delta`)
- Content-block detection for text vs tool_use

## OpenAI Provider

**Config:** `config:ryeos-runtime/model-providers/openai`

### Tier Mapping
| Tier     | Model          |
|----------|----------------|
| fast     | gpt-4o-mini    |
| general  | gpt-4o-mini    |
| high     | gpt-4o         |
| max      | o3             |

### Message Format
- `message_role` system message mode
- Tool results use `direct` wrap with `tool` role
- Tool calls use `function` type with `name`/`arguments`

### Streaming
- Mode: `delta_merge` (merge delta chunks)
- Sentinel: `[DONE]`

## Zen Provider (Primary Gateway)

**Config:** `config:ryeos-runtime/model-providers/zen`
**The primary provider** — all routing table tiers use Zen.

### Profiles
Zen uses a profile system that deep-merges provider-specific config
over the shared base:

#### `anthropic` profile — matches `claude-*`
- URL: `/zen/v1/messages`
- Auth: `x-api-key` header
- Full Anthropic schema (blocks_array, event_typed streaming)

#### `openai_compat` profile — matches `gpt-*`, `minimax-*`, `glm-*`, `kimi-*`, `qwen*`, etc.
- URL: `/zen/v1/chat/completions`
- Standard OpenAI schema (separate content, delta_merge streaming)

#### `gemini` profile — matches `gemini-*`
- URL: `/zen/v1/models/{model}:streamGenerateContent?alt=sse`
- Auth: `x-goog-api-key` header
- Google schema (blocks mode, `complete_chunks` streaming)
- Includes `thinkingConfig` for Gemini's thinking feature

### Pricing
The Zen adapter includes pricing for 30+ models across all families.
Free models (minimax-m2.5-free, big-pickle, trinity-large-preview-free,
nemotron-3-super-free, hy3-preview-free) have zero cost.

## How Providers Are Used

During directive execution:
1. The directive runtime reads the routing table to find the model
   and provider for the directive's tier
2. It loads the provider config and resolves profile overrides
3. The runtime builds the HTTP request (messages + tools) using the
   provider's wire format (Anthropic blocks, OpenAI chat, Gemini
   generate)
4. The runtime handles streaming response parsing via the provider's
   streaming mode (event_typed, delta_merge, complete_chunks)
5. Tool calls are detected and dispatched through the daemon
6. Tool results are formatted and re-submitted

This happens automatically — directive authors only need to specify
`model.tier` (or accept the default).

Provider configs are signed YAML files under
`config/ryeos-runtime/model-providers/`. Adding a provider means
adding a new signed config and pointing a routing tier at it. See
[model-providers](model-providers.md) for details.
