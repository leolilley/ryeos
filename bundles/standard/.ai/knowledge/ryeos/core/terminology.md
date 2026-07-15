<!-- ryeos:signed:2026-07-15T07:49:20Z:3b063382492e5656927a12567a2964e65cff11f4fd7db58ee953552d206648cf:0d51MyxjVD4Dy8EGjEqB8Kt6ac5q+OBen/UrnoRram55yHpEDlJ7f+AN6mxQm7VJwEWhdR0gTKQWEYGjiiGXCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core
tags: [reference, terminology, naming, conventions]
version: "1.0.0"
description: >
  Terminology and naming conventions for the Rust Rye OS system —
  binary names, namespaces, kind names, case rules, and key terms.
---

# Terminology and Naming Conventions

## Binary Names

The Rust workspace produces 14 executables across 8 crates:

| Binary                        | Crate                    | Purpose                          |
|-------------------------------|--------------------------|----------------------------------|
| `ryeos`                       | `ryeos-cli`              | Operator CLI                     |
| `ryeosd`                      | `ryeosd`                 | Daemon (control plane)           |
| `ryeos-graph-runtime`         | `ryeos-graph-runtime`    | Graph subprocess runtime         |
| `ryeos-directive-runtime`     | `ryeos-directive-runtime`| Directive subprocess runtime     |
| `ryeos-knowledge-runtime`     | `ryeos-knowledge-runtime`| Knowledge subprocess runtime     |
| `ryeos-core-tools`            | `ryeos-tools`            | Unified tools (sign/fetch/verify)|
| `rye-composer-extends-chain`  | `ryeos-handler-bins`     | Composer handler subprocess      |
| `rye-composer-graph-permissions` | `ryeos-handler-bins`  | Composer handler subprocess      |
| `rye-composer-identity`       | `ryeos-handler-bins`     | Composer handler subprocess      |
| `rye-parser-yaml-document`    | `ryeos-handler-bins`     | Parser handler subprocess        |
| `rye-parser-yaml-header-document` | `ryeos-handler-bins` | Parser handler subprocess        |
| `rye-parser-regex-kv`         | `ryeos-handler-bins`     | Parser handler subprocess        |
| `lillux`                      | `lillux`                  | Kernel primitives (CAS, crypto, exec, identity, time) |

Two naming prefixes coexist:
- **`ryeos`** — main system binaries (CLI, daemon, runtimes, tools)
- **`rye-`** — handler subprocess binaries (parsers and composers)
- **`lillux`** — kernel primitives (CAS, crypto, exec, identity, time)

## Namespace Convention

The canonical namespace is `ryeos/core/` (not `rye/core/`):

```yaml
category: "ryeos/core"                    # core tools, handlers, protocols
category: "ryeos/core/subprocess"         # subprocess executor
category: "ryeos/core/yaml"               # YAML parser
category: "ryeos/agent/providers/zen"     # Zen adapter (standard bundle)
category: "ryeos-runtime"                 # runtime config
category: "engine/kinds/tool"             # kind schemas
```

Canonical refs follow the category namespace:
```
tool:ryeos/core/sign
handler:ryeos/core/extends-chain
parser:ryeos/core/yaml/yaml
protocol:ryeos/core/runtime
```

## Kind Names

12 kinds, all lowercase. Multi-word kinds use `snake_case`:

```
config        directive       graph
handler       knowledge       node
parser        protocol        runtime
service       streaming_tool  tool
```

`tool` and `streaming_tool` share the same directory (`tools/`).
Differentiation is by execution protocol, not file location.

## MCP Interface

The MCP server (`ryeosd-mcp`, package `ryeosd-mcp`) exposes a
**single tool** named `cli`:

```json
{"tool": "cli", "args": ["fetch", "tool:ryeos/core/sign"], "project_path": "/path"}
```

The server discovers the CLI binary in order:
1. `RYE_BIN` environment variable
2. `shutil.which("ryeos")`

## CLI Verbs

Daemon verbs are **kebab-case** and are merged from installed bundles. Core
contributes control-plane verbs; standard contributes workflow verbs.

```
bundle-install              bundle-list              bundle-remove
commands-submit             compose
events-chain-replay         events-replay            execute
fetch                       identity-public-key      maintenance-gc
rebuild                     remote-authorize         remote-bundle-install
remote-configure            remote-execute           remote-list
remote-pull                 remote-push              remote-status
remote-thread-status        remote-threads           remote-vault-delete
remote-vault-list           remote-vault-set         sign
status                      vault-delete             vault-list
vault-set                   verify
scheduler-deregister        scheduler-list           scheduler-pause
scheduler-register          scheduler-resume         scheduler-show-fires
thread-chain                thread-children          thread-get
thread-list                 thread-tail
```

Plus local verbs (no daemon needed):
```
ryeos init
ryeos trust pin
ryeos publish
ryeos vault put / list / remove / rewrap
```

## Case Conventions

| Domain               | Convention      | Examples                          |
|----------------------|-----------------|-----------------------------------|
| Crate names          | kebab-case      | `ryeos-cli`, `ryeos-engine`       |
| Rust source files    | snake_case      | `canonical_ref.rs`, `plan_builder.rs` |
| Main binaries        | kebab-case      | `ryeos`, `ryeosd`                 |
| Handler binaries     | `rye-` + kebab  | `rye-composer-identity`           |
| Kind names           | lowercase       | `tool`, `streaming_tool`          |
| YAML keys            | snake_case      | `binary_ref`, `required_caps`     |
| CLI verbs            | kebab-case      | `bundle-install`, `thread-list`   |
| CLI aliases          | single letter   | `s` → sign, `f` → fetch           |
| Canonical ref paths  | lowercase + `/` | `ryeos/core/verify`               |
| Environment vars     | UPPER_SNAKE     | `RYEOS_APP_ROOT`                  |
| Protocol names       | snake_case      | `tool_streaming`, `runtime` |
| Model tiers          | snake_case      | `code_max`, `vision_fast`         |
| Category values      | slash-separated | `ryeos/core`, `engine/kinds/tool` |
| Node sections        | flat lowercase  | `verbs`, `aliases`, `routes`      |

## Key Terms

| Term              | Definition                                          |
|-------------------|-----------------------------------------------------|
| **Item**          | Any file in `.ai/` with typed metadata              |
| **Kind**          | Schema + behavior contract for an item type         |
| **Canonical ref** | `kind:path/to/item` — structured item address       |
| **Bare ID**       | `path/to/item` without kind prefix (auto-detects)   |
| **Bundle**        | Signed `.ai/` tree distributed as a unit            |
| **Space**         | Resolution tier: project, user, or system           |
| **CAS**           | Content-addressed storage for items and events       |
| **Thread**        | Tracked execution unit with ID, events, lifecycle   |
| **Verb**          | CLI command name mapped to a service or tool        |
| **Alias**         | CLI token shortcut mapping to a verb                |
| **Handler**       | Subprocess binary for parsing or composing items    |
| **Parser**        | Handler that extracts metadata from source files    |
| **Composer**      | Handler that merges/inherits item metadata          |
| **Runtime**       | Subprocess binary that executes directives/graphs   |
| **Protocol**      | Wire contract defining subprocess communication     |
| **Service**       | In-process daemon endpoint                          |
| **Route**         | HTTP endpoint served by the daemon                  |
| **Node**          | A single daemon instance with its identity and state|
| **Tier**          | Abstract model capability level (fast, high, max…)  |
| **Provider**      | LLM API endpoint configuration                     |
| **Profile**       | Provider sub-config matching model name patterns    |
