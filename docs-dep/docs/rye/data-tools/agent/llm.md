**Source:** Original implementation: `.ai/tools/rye/llm/` in kiwi-mcp

# LLM Category

## Purpose

LLM configurations define **provider-specific settings** for different LLM services.

**Location:** `.ai/tools/rye/llm/`  
**Count:** 5 YAML configs  
**Type:** Configuration files (not executable tools)

## Core LLM Configurations

### 1. OpenAI Chat (`openai_chat.yaml`)

```yaml
name: openai_chat
version: "1.0.0"
provider: openai
api_type: chat_completion
category: llm

config:
  model: gpt-4-turbo
  temperature: 0.7
  top_p: 1.0
  max_tokens: 4096
  presence_penalty: 0
  frequency_penalty: 0

env_config:
  api_key:
    type: credential
    key: "openai_api_key"
    var: "OPENAI_API_KEY"
  api_base:
    type: url
    var: "OPENAI_API_BASE"
    fallback: "https://api.openai.com/v1"
```

**Use Case:** ChatGPT API with streaming responses

### 2. OpenAI Completion (`openai_completion.yaml`)

```yaml
name: openai_completion
version: "1.0.0"
provider: openai
api_type: text_completion
category: llm

config:
  model: text-davinci-003
  temperature: 0.7
  max_tokens: 2048
  top_p: 1.0
  frequency_penalty: 0.0
  presence_penalty: 0.0

env_config:
  api_key:
    type: credential
    key: "openai_api_key"
    var: "OPENAI_API_KEY"
```

**Use Case:** Legacy text completion endpoint

### 3. Anthropic Messages (`anthropic_messages.yaml`)

```yaml
name: anthropic_messages
version: "1.0.0"
provider: anthropic
api_type: messages
category: llm

config:
  model: claude-3-opus-20240229
  temperature: 1.0
  max_tokens: 4096
  thinking:
    type: enabled
    budget_tokens: 10000
  tools: []

env_config:
  api_key:
    type: credential
    key: "anthropic_api_key"
    var: "ANTHROPIC_API_KEY"
  api_base:
    type: url
    var: "ANTHROPIC_API_BASE"
    fallback: "https://api.anthropic.com"
```

**Use Case:** Claude API with tool use and extended thinking

### 4. Anthropic Completion (`anthropic_completion.yaml`)

```yaml
name: anthropic_completion
version: "1.0.0"
provider: anthropic
api_type: completion
category: llm

config:
  model: claude-instant-1.3
  temperature: 1.0
  max_tokens_to_sample: 1024
  prompt_template: "{prompt}"
  stop_sequences: ["\n"]

env_config:
  api_key:
    type: credential
    key: "anthropic_api_key"
    var: "ANTHROPIC_API_KEY"
```

**Use Case:** Legacy Claude completion mode

### 5. Pricing (`pricing.yaml`)

```yaml
name: pricing
version: "1.0.0"
category: llm
type: pricing_table

config:
  openai:
    gpt-4-turbo:
      input: 0.01  # per 1K tokens
      output: 0.03
    gpt-3.5-turbo:
      input: 0.0005
      output: 0.0015
    text-davinci-003:
      input: 0.02
      output: 0.04

  anthropic:
    claude-3-opus:
      input: 0.015
      output: 0.075
    claude-3-sonnet:
      input: 0.003
      output: 0.015
    claude-instant:
      input: 0.00081
      output: 0.0024
```

**Use Case:** Token pricing calculation and cost estimation

## LLM Configuration Structure

Each configuration includes:

```yaml
name: provider_model
version: "1.0.0"
provider: openai | anthropic
category: llm

config:                        # Provider-specific settings
  model: "..."
  temperature: 0.7
  max_tokens: 4096
  ...

env_config:                    # Environment variable resolution
  api_key:
    type: credential
    key: "..."
    var: "..."
```

## Provider-Specific Features

### OpenAI
- **Chat Completion:** `/v1/chat/completions`
- **Text Completion:** `/v1/completions` (legacy)
- **Streaming:** Supported
- **Tools:** Function calling

### Anthropic
- **Messages API:** Preferred interface
- **Tool Use:** Native support
- **Extended Thinking:** Claude 3 models
- **Vision:** Claude 3 models
- **Streaming:** Supported

## Usage Examples

### Select OpenAI Chat

```bash
# In RYE configuration or tool
llm_config: openai_chat
model: gpt-4-turbo
temperature: 0.7
max_tokens: 4096
```

### Select Anthropic Messages

```bash
# In RYE configuration or tool
llm_config: anthropic_messages
model: claude-3-opus-20240229
max_tokens: 4096
thinking_budget: 10000
```

### Calculate Costs

```bash
# Using pricing configuration
tokens_used:
  input: 5000
  output: 2000
provider: openai
model: gpt-4-turbo

cost = (5000 / 1000) * 0.01 + (2000 / 1000) * 0.03
     = 0.05 + 0.06
     = $0.11
```

## Environment Resolution

LLM configs use env_config to resolve credentials:

```
LLM Tool invocation
    │
    ├─→ Load llm config (e.g., openai_chat.yaml)
    ├─→ Get ENV_CONFIG
    │
    ├─→ env_resolver.resolve()
    │   ├─ Find OPENAI_API_KEY credential
    │   ├─ Resolve OPENAI_API_BASE URL
    │   └─ Return: {"OPENAI_API_KEY": "...", ...}
    │
    └─→ Initialize LLM client with resolved config
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Count** | 5 configs |
| **Location** | `.ai/tools/rye/llm/` |
| **Type** | Configuration files (YAML) |
| **Purpose** | LLM provider settings |
| **Providers** | OpenAI, Anthropic |
| **Not Executable** | Used to configure tools/threads |

## Related Documentation

- [overview](../overview.md) - All categories
- [agent/threads](threads.md) - Threads use LLM configs
- [../bundle/structure](../bundle/structure.md) - Bundle organization
