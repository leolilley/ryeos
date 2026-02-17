<!-- rye:signed:2026-02-17T23:54:02Z:2e76cda69ef782d8371b894be4deb046e7cb41b42650c7cf1d10b74595f3e9cb:WndftnIGdiTTFLzn9TUPUhNyn5VeVZ6eQID9KYlhYVf4cqSWkfb5Ut1JoAvI8f9UgzKKLvwwhhNQymDMlm2yAw==:440443d0858f0199 -->

```yaml
id: provider-configuration
title: Provider Configuration
entry_type: reference
category: rye/agent
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - providers
  - models
  - configuration
references:
  - threads/thread-lifecycle
  - "docs/orchestration/overview.md"
```

# Provider Configuration

How providers and models are configured, resolved, and used by the threading system.

## Model Tiers

Tiers abstract over specific models. Directives declare a tier; the provider resolver maps it to a concrete model.

| Tier           | Purpose                                    | Typical Model                  |
|----------------|--------------------------------------------|--------------------------------|
| `low`          | Cheap/fast for simple tasks (file writes)  | `claude-3-5-haiku-20241022`    |
| `haiku`        | Alias for `low`                            | `claude-3-5-haiku-20241022`    |
| `sonnet`       | Reasoning for moderate orchestration       | `claude-sonnet-4-20250514`     |
| `general`      | Alias for `sonnet`                         | `claude-sonnet-4-20250514`     |
| `orchestrator` | Complex multi-step workflows with spawning | `claude-sonnet-4-20250514`     |

## Declaring in Directives

```xml
<!-- Simple task — cheap and fast -->
<model tier="low" />

<!-- With specific model ID override -->
<model tier="haiku" id="claude-3-5-haiku-20241022" />

<!-- Complex orchestration — needs reasoning -->
<model tier="orchestrator" fallback="general" />
```

### Fallback

The `fallback` attribute specifies the tier to try if the primary tier's model is unavailable. The `fallback="general"` pattern is common for orchestrators.

## Model Resolution Order

When `thread_directive` creates a thread, the model is resolved in this priority:

```
params.model → directive.model.id → directive.model.tier → default
```

1. **`params.model`** — explicit model passed in `thread_directive` parameters
2. **`directive.model.id`** — specific model ID in the directive XML (`id="claude-3-5-haiku-20241022"`)
3. **`directive.model.tier`** — tier string resolved via provider_resolver
4. **Default** — falls back to the project's default model

## Provider Resolver

The `provider_resolver` maps a tier string to a concrete model configuration:

- Reads provider config from `.ai/config/providers.yaml` (project-level) or system defaults
- Looks up the tier in the provider's model table
- Returns an `HttpProvider` instance with the resolved model, API endpoint, and credentials

## API Key Setup

API keys are loaded from environment variables, never stored in config files:

| Provider   | Environment Variable    |
|------------|------------------------|
| Anthropic  | `ANTHROPIC_API_KEY`    |
| OpenAI     | `OPENAI_API_KEY`       |

Keys are read at provider construction time. Missing keys cause an immediate error before the LLM loop starts.

## Directive Role → Model Mapping

Standard patterns for choosing tiers based on directive role:

| Role             | Tier         | Turns | Spend  | Why                                        |
|------------------|--------------|-------|--------|--------------------------------------------|
| Root orchestrator| `sonnet`     | 30    | $3.00  | Multi-phase coordination, state reasoning  |
| Sub-orchestrator | `sonnet`     | 20    | $1.00  | Spawn/wait/aggregate cycle                 |
| Strategy leaf    | `haiku`      | 6     | $0.05  | Load knowledge + state, decide action      |
| Execution leaf   | `haiku`      | 4–10  | $0.10  | Call one tool, save output, return          |

### Cost Implications

- Orchestrators use expensive reasoning models but run few turns (~$0.30 for 30 sonnet turns)
- Leaves use cheap models — a leaf at 4 turns costs ~$0.01–0.03 with haiku
- A pipeline with 20 haiku leaves costs ~$0.40–0.60 vs ~$3.00+ if run in one sonnet conversation

## Provider YAML Format

```yaml
# .ai/config/providers.yaml
providers:
  anthropic:
    api_base: "https://api.anthropic.com"
    models:
      low: claude-3-5-haiku-20241022
      haiku: claude-3-5-haiku-20241022
      sonnet: claude-sonnet-4-20250514
      general: claude-sonnet-4-20250514
      orchestrator: claude-sonnet-4-20250514
    default_tier: haiku
```

## HttpProvider

The resolved model configuration is used to construct an `HttpProvider`:

- Holds the model ID, API base URL, API key, and context window size
- Exposes `create_completion(messages, tools)` for LLM calls
- Reports `input_tokens`, `output_tokens`, and `spend` per call
- `context_window` is used by the runner to detect context limit thresholds
