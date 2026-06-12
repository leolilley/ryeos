<!-- ryeos:signed:2026-06-11T21:03:05Z:2d091b29cfff55a36f94982357cdf5d9e7e75bcb794bf99ab597bdbb61374675:G+DJmuC3i0OIJ1ubVDYRf60X3zA9rvp/Bgi2Q7ADu6i5KbLuJWmvC1HafLHGE9D8fW7SXSOIelLL6HmruiCXDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config/providers
tags: [provider, openrouter, gateway, models]
version: "1.0.0"
description: OpenRouter provider config reference.
---

# Provider Config: openrouter

OpenRouter provides a unified OpenAI-compatible API that routes to hundreds of models across providers (Anthropic, OpenAI, Google, Meta, DeepSeek, Qwen, xAI, Mistral, and more).

The provider uses the `chat_completions` family with the standard OpenAI-compatible request/response shape. Authentication is via the `OPENROUTER_API_KEY` environment variable. Model IDs use the `provider/model` format (e.g. `anthropic/claude-sonnet-4-5`, `openai/gpt-5-4`, `google/gemini-2-5-pro`).

Pricing is per-model and pulled from OpenRouter's live pricing. The `available_models` extra field organizes models by family for UI/CLI discovery.
