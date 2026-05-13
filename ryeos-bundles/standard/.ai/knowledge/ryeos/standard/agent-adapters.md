---
category: ryeos/standard
tags: [reference, providers, zen, anthropic, openai, adapter]
version: "1.0.0"
description: >
  The agent adapter tools — full HTTP client specifications for
  calling LLM providers with request building, response parsing,
  and streaming support.
---

# Agent Adapter Tools

The standard bundle includes three agent adapter tools that serve as
complete HTTP client specifications for LLM providers. These adapters
are used by the directive runtime during the prompt + tool loop.

## Adapter Architecture

Each adapter is a YAML tool definition that specifies:
- **How to build requests** (URL, headers, body template)
- **How to parse responses** (content extraction, tool call detection)
- **How to handle streaming** (SSE event types, delta merging)
- **How to format tool results** (wrapping for re-submission)
- **Pricing** (per-model cost per million tokens)

## Anthropic Adapter (`tool:ryeos/agent/providers/anthropic`)

**Version:** 1.3.0
**Type:** `http` (makes outbound HTTP calls)
**Requires:** `net.call`

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

## OpenAI Adapter (`tool:ryeos/agent/providers/openai`)

**Version:** 1.0.0
**Type:** `http`
**Requires:** `net.call`

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

## Zen Adapter (`tool:ryeos/agent/providers/zen`)

**Version:** 1.0.0
**Type:** `http`
**Requires:** `net.call`
**The primary adapter** — all routing table tiers use Zen.

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

## How Adapters Are Used

During directive execution:
1. The directive runtime reads the routing table to find the model
   and provider for the directive's tier
2. It selects the appropriate agent adapter
3. The adapter builds the HTTP request (messages + tools)
4. The adapter handles streaming response parsing
5. Tool calls are detected and dispatched through the daemon
6. Tool results are formatted and re-submitted

This happens automatically — directive authors only need to specify
`model.tier` (or accept the default).
