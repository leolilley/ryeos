---
category: "ryeos/reference"
name: "bundle-contents"
description: "What's in the core and standard bundles — items, binaries, and structure"
---

# Bundle Contents

## Core bundle (`ryeos-bundles/core/`)

Infrastructure the daemon needs to function. Signed with the dev publisher key.

### Kind schemas (`.ai/node/engine/kinds/`)

Define all item types the system understands:

| Kind | Purpose |
|---|---|
| `directive` | Workflow instructions with YAML frontmatter |
| `tool` | Executable tool descriptors |
| `knowledge` | Domain information |
| `graph` | State graph definitions |
| `service` | Operational services |
| `runtime` | Execution runtime definitions |
| `handler` | Execution strategy definitions |
| `protocol` | Wire contract definitions |
| `parser` | File format parser definitions |
| `streaming_tool` | Streaming tool descriptors |
| `config` | Configuration items |
| `node` | Node configuration items |
| `kind_schema` | Kind schema definitions (meta) |

### Parsers (`.ai/parsers/`)

| Parser | Parses |
|---|---|
| `yaml/yaml` | Plain YAML documents |
| `yaml-header-document` | YAML header + body (signature line + YAML + content) |
| `markdown/frontmatter` | Markdown with YAML frontmatter |
| `markdown/directive` | Directive format (frontmatter + body) |
| `python/ast` | Python files via AST |
| `javascript/javascript` | JavaScript/TypeScript files via regex |

### Handlers (`.ai/handlers/`)

| Handler | Strategy |
|---|---|
| `yaml-document` | Parse YAML documents |
| `yaml-header-document` | Parse YAML with header (signature + metadata) |
| `regex-kv` | Parse key-value via regex |
| `identity` | Pass-through (no transformation) |
| `extends-chain` | Walk extends chain for composition |
| `graph-permissions` | Graph-specific permission composition |

### Protocols (`.ai/protocols/`)

| Protocol | Purpose |
|---|---|
| `runtime_v1` | Standard runtime subprocess communication |
| `tool_streaming_v1` | Streaming tool output |
| `opaque` | Opaque token passing |

### Services (`.ai/services/`)

| Service | Purpose |
|---|---|
| `fetch` | Item fetch by ref or query |
| `sign` | Item signing |
| `verify` | Item verification |
| `identity/public_key` | Public key retrieval |
| `rebuild` | Item rebuild |
| `health/status` | Health check |
| `system/status` | System status |
| `events/replay` | Event replay |
| `events/chain_replay` | Chain event replay |

### Core tools (`.ai/tools/`)

| Tool | Purpose |
|---|---|
| `ryeos/core/sign` | Sign items |
| `ryeos/core/verify` | Verify item signatures |
| `ryeos/core/fetch` | Fetch items by ref or query |
| `ryeos/core/identity/public_key` | Public key retrieval |
| `ryeos/core/subprocess/execute` | Execute subprocess |
| `ryeos/core/verbs/list` | List available verbs |
| Runtime descriptors | `runtimes/{bash,python/{script,function},state-graph/runtime}.yaml` |

### Binaries (`.ai/bin/<triple>/`)

| Binary | Purpose |
|---|---|
| `ryeos-core-tools` | Core tool binary (sign, verify, fetch, identity) |
| `rye-parser-yaml-document` | YAML document parser |
| `rye-parser-yaml-header-document` | YAML header document parser |
| `rye-parser-regex-kv` | Regex KV parser |
| `rye-composer-extends-chain` | Extends chain composer |
| `rye-composer-graph-permissions` | Graph permissions composer |
| `rye-composer-identity` | Identity composer |

### Routes (`.ai/node/routes/`)

| Route | Endpoint |
|---|---|
| `execute` | POST /execute |
| `execute-stream` | POST /execute (streaming) |
| `health` | GET /health |
| `public-key` | GET /public-key |
| `threads-detail` | Thread management |
| `threads-cancel` | Thread cancellation |
| `thread-events-stream` | Thread event streaming |

### Verbs + Aliases (`.ai/node/verbs/`, `.ai/node/aliases/`)

See [CLI Verbs](cli-verbs.md) for the full list.

---

## Standard bundle (`ryeos-bundles/standard/`)

User-facing runtimes, model providers, and example directives.

### Runtimes (`.ai/runtimes/`)

| Runtime | Purpose |
|---|---|
| `directive-runtime` | Execute directives via LLM loop |
| `graph-runtime` | Execute state graph nodes |
| `knowledge-runtime` | Compose knowledge entries |

### Model providers (`.ai/config/ryeos-runtime/model-providers/`)

| Provider | Config |
|---|---|
| `anthropic.yaml` | Anthropic API (Claude models) |
| `openai.yaml` | OpenAI API |
| `openrouter.yaml` | OpenRouter API |
| `zen.yaml` | Zen API |

### Model routing (`.ai/config/ryeos-runtime/model_routing.yaml`)

Maps model tiers (fast, general, orchestrator) to specific model IDs per provider.

### Provider tools (`.ai/tools/ryeos/agent/providers/`)

LLM provider adapter tools that the directive runtime uses to make API calls.

### Directives (`.ai/directives/`)

| Directive | Purpose |
|---|---|
| `hello.md` | Minimal smoke test — single LLM round-trip, no tool dispatch |

### Binaries (`.ai/bin/<triple>/`)

| Binary | Purpose |
|---|---|
| `ryeos-directive-runtime` | Directive execution |
| `ryeos-graph-runtime` | State graph execution |
| `ryeos-knowledge-runtime` | Knowledge composition |
