---
category: ryeos/core
tags: [reference, templating, interpolation, substitution]
version: "1.0.0"
description: >
  The six distinct interpolation/template systems in Rye OS — where each
  runs, what syntax it uses, and what variables are available.
---

# Templating and Interpolation

Rye OS has **six** distinct template/interpolation systems, each running
in a different execution context. They are intentionally syntactically
distinct to prevent collisions.

## Where Each System Runs

```
┌─────────────────────────────────────────────────────────────┐
│  RUST DAEMON (ryeosd / ryeos-engine)                        │
│                                                             │
│  System 1: {token}         — tool config.command/args       │
│  System 2: ${VAR} + {token} — env_config values             │
│  System 3: ${path.X}       — HTTP route source_config       │
├─────────────────────────────────┬───────────────────────────┤
│                                 │ fork + exec               │
│                                 ▼                           │
│  RUST RUNTIME SUBPROCESS                                    │
│  (ryeos-graph-runtime, ryeos-directive-runtime)             │
│                                                             │
│  System 4: ${path} + {input:name} — graph/directive bodies │
│  System 5: {key} exact-match      — provider API templates │
├─────────────────────────────────┬───────────────────────────┤
│                                 │ legacy (being replaced)   │
│                                 ▼                           │
│  PYTHON SUBPROCESS                                           │
│  System 6: ${path} + {input:name} — state-graph walker     │
└─────────────────────────────────────────────────────────────┘
```

---

## System 1: Engine `{token}` Expansion

**Runs in:** Rust daemon (`ryeosd`), during the `compile_with_handlers` pipeline.
**Syntax:** `{token}` — simple brace-delimited replacement.
**Unknown tokens:** Hard error at plan-build time.

### Available Tokens

| Token            | Value                                        |
|------------------|----------------------------------------------|
| `{tool_path}`    | Absolute path to the tool source file        |
| `{tool_dir}`     | Parent directory of the tool source file     |
| `{tool_parent}`  | Grandparent of the tool source file          |
| `{project_path}` | Absolute path to the project root            |
| `{params_json}`  | Full parameters as JSON string               |
| `{interpreter}`  | Resolved Python binary (from env_config)     |
| `{runtime_dir}`  | Current chain element's directory            |

### Where Used

In tool YAML `config` blocks:
```yaml
config:
  command: "{interpreter}"
  args: ["{tool_path}", "--project-path", "{project_path}"]
  input_data: "{params_json}"
  cwd: "{tool_dir}"
```

And in `env_config.env_paths`:
```yaml
env_config:
  env_paths:
    PYTHONPATH:
      prepend: ["{tool_dir}", "{runtime_dir}/lib"]
```

---

## System 2: Engine Env Value Two-Pass Expansion

**Runs in:** Rust daemon, during `compile_with_handlers` — env values only.
**Syntax:** Two passes — `${VAR}` first, then `{token}`.

### Pass 1: `${VAR}` — Host Environment Passthrough
Resolves `VAR` from the operator's environment. Only allowed for
variables in the `RYEOS_TOOL_ENV_PASSTHROUGH` allowlist. Reserved
`RYEOS_*` names are rejected.

### Pass 2: `{token}` — Same as System 1
After host-env expansion, the same `{token}` expansion runs.

### Where Used

`env_config.env` values and `env_config.env_paths` values:
```yaml
env_config:
  env:
    PATH: "${PATH}"                     # Pass 1: host env
    PYTHONUNBUFFERED: "1"               # Literal (no expansion)
    PROJECT_VENV_PYTHON: "{interpreter}" # Pass 2: engine token
  env_paths:
    PYTHONPATH:
      prepend: ["{tool_dir}", "{runtime_dir}/lib"]
```

### Design Note
`${VAR}` (host env) and `{token}` (engine context) are intentionally
syntactically distinct. A lone `$` without braces passes through
(e.g., `"price: $5"`).

---

## System 3: Route `source_config` Path Interpolation

**Runs in:** Rust daemon, during route table compilation and HTTP dispatch.
**Syntax:** `${path.<name>}` — only `path.*` captures are supported.

### What It Does
At daemon startup, validates that every `${path.X}` in a route's
`source_config` references a capture group declared in the route pattern.
At request time, substitutes actual capture values.

### Where Used

Route YAML `response.source_config`:
```yaml
response:
  mode: json
  source_config:
    thread_id: "${path.thread_id}"
    project_path: "/some/project"
```

For a route with `path: /threads/{thread_id}`, the `${path.thread_id}`
is replaced with the actual thread ID from the URL.

### Unsupported (Rejected at Startup)
`${headers.*}`, `${body.*}` — only path captures exist in Phase 1.

---

## System 4: Rust Graph/Directive Interpolation

**Runs in:** Rust runtime subprocesses (`ryeos-graph-runtime`,
`ryeos-directive-runtime`).
**Syntax:** Two distinct forms.

### Form A: `${dotted.path}` — Context Path Resolution

| Syntax               | Meaning                              |
|----------------------|--------------------------------------|
| `${state.x}`         | Value from current graph state       |
| `${inputs.target}`   | Value from execution inputs          |
| `${result.deploy.status}` | Value from a previous node result |
| `${a.b \|\| c.d}`   | Fallback chain — first non-None wins |

Built-in variables: `${_now}` (ISO timestamp), `${_timestamp}` (epoch).

### Form B: `{input:name}` — Input Parameter Reference

| Syntax                  | Behavior                        |
|-------------------------|---------------------------------|
| `{input:name}`          | **Hard error** if missing       |
| `{input:name?}`         | Returns `""` if missing         |
| `{input:name:default}`  | Returns `default` if missing    |
| `{input:name\|default}` | Returns `default` if missing    |

**Important:** Unlike the legacy Python system, missing inputs without a
modifier (`?`, `:`, or `|`) produce a **hard error** with a clear message
suggesting the fix.

### Pipe Filters
`${value \| filter}` — apply transformations:

| Filter      | Description                        |
|-------------|------------------------------------|
| `json`      | Serialize to JSON string           |
| `from_json` | Parse JSON string                  |
| `length`    | Length of array/string             |
| `keys`      | Keys of an object                  |
| `upper`     | Uppercase                          |
| `lower`     | Lowercase                          |
| `type`      | JSON type name (new in Rust)       |

### Type Preservation
When the entire template string is exactly `${path}`, the resolved value
retains its JSON type (number, array, object) without stringification.

### Where Used
- Graph YAML: node actions, params, assign, foreach over, edge conditions
- Directive prompts: `{input:target}` in the body text
- Hook definitions: condition and action interpolation

---

## System 5: Provider Template Substitution

**Runs in:** Rust directive runtime, during LLM API call construction.
**Syntax:** `{key}` — exact **whole-string** match only.

### What It Does
Recursively walks a JSON template and replaces any string whose **entire
trimmed content** is `{key}` with `data[key]`, preserving the value's
JSON type. A string like `"Hello {name}"` would NOT be interpolated —
only `"{name}"` (the whole string) matches.

### Where Used
Provider adapter message serialization and tool schema formatting:
```json
{"type": "function", "function": {"name": "{name}", "parameters": "{input_schema}"}}
```

### Error Behavior
Missing placeholders become `null` with a warning log (not an error).

---

## System 6: Python Graph Interpolation (Legacy)

**Runs in:** Python subprocess (state-graph walker).
**Syntax:** Identical to System 4 (same `${...}` and `{input:...}` forms).

### Difference from Rust (System 4)
The Python version is more lenient: missing `{input:name}` without a
modifier silently returns the original `{input:name}` text instead of
erroring. The Rust version (System 4) hard-errors on missing inputs.

### Status
The Python walker is being replaced by the Rust graph runtime. System 6
exists for backward compatibility during the transition.

---

## Execution Order

When all systems could apply to the same data, they run in this order:

1. **Daemon compile time:** System 2 (env `${VAR}`) → System 1 (`{token}`)
2. **Route dispatch time:** System 3 (`${path.X}`)
3. **Runtime subprocess:** System 4/6 (`${dotted.path}`, `{input:name}`)
4. **Provider formatting:** System 5 (`{key}` exact-match)

Each system operates on different data at different stages, so there
are no ordering conflicts within a single stage.

## Collision Prevention

The syntaxes are designed to be distinguishable:

| Syntax         | System          | Context            |
|----------------|-----------------|--------------------|
| `{token}`      | System 1, 2     | Daemon config only |
| `${VAR}`       | System 2        | Env values only    |
| `${path.X}`    | System 3        | Route configs only |
| `${dotted.path}` | System 4, 5, 6 | Runtime bodies    |
| `{input:name}` | System 4, 6     | Runtime bodies    |
| `{key}`        | System 5        | Provider templates |
