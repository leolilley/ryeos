<!-- ryeos:signed:2026-07-15T07:49:20Z:72792d79dea4aec177caccf4f812fec0a8bd809467d511ff08ecf4dc48f59c80:cqGfBarVlyZReXVucs/bdb2499wTyuoQjilYk2hOqSLIUQ4Fz50J8m1Lx4WBwGIO2VEtVa9IkdfRGeFc8jg4AA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core
tags: [reference, templating, interpolation, substitution]
version: "1.0.0"
description: >
  The five distinct interpolation/template systems in Rye OS — where each
  runs, what syntax it uses, and what variables are available.
---

# Templating and Interpolation

Rye OS has **five** distinct template/interpolation systems, each running
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
├─────────────────────────────────────────────────────────────┤
│                                 │ fork + exec               │
│                                 ▼                           │
│  RUST RUNTIME SUBPROCESS                                    │
│  (ryeos-graph-runtime, ryeos-directive-runtime)             │
│                                                             │
│  System 4: rye-expr/1 ${expression} — runtime bodies       │
│  System 5: {key} exact-match      — provider API templates │
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

The Python runtimes no longer set `PYTHONPATH`; they derive
bundle-local import roots from `{tool_path}` and prepend them to
`sys.path` inside the runtime launcher.

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
    PATH:
      prepend: ["{runtime_dir}/bin"]
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
**Syntax:** `${expression}` using the `rye-expr/1` grammar.

### Paths, Literals, and Nullish Fallback

| Syntax                                  | Meaning                                      |
|-----------------------------------------|----------------------------------------------|
| `${state.x}`                            | Value from current graph state               |
| `${inputs.target}`                      | Value from execution inputs                  |
| `${result.status}`                      | Result of the current action, where available |
| `${inputs.target ?? "default"}`         | Use the fallback when the left is missing or null |
| `${state.primary ?? state.backup ?? []}` | Chain nullish fallbacks                      |

Expressions may contain JSON literals: strings, numbers, booleans, `null`,
arrays, and objects. `??` is nullish, not truthy: `false`, `0`, `""`, and
`[]` remain valid values and do not select the fallback.

Paths support dot access and dynamic bracket access such as
`${records[index].name}`. Array indexes must be non-negative integers and
object indexes must be strings. A missing path is an error unless `??` or
`exists(path)` handles it; wrong-typed traversal remains an error.

### Operators

The language provides unary `!`, `+`, and `-`; arithmetic `+`, `-`, `*`, `/`,
and `%`; deep equality `==` and `!=`; ordering `<`, `<=`, `>`, and `>=`;
membership `in`; strict boolean `&&` and `||`; nullish `??`; and the ternary
`condition ? then : else`. Boolean operators require booleans. Ordering accepts
number/number or string/string pairs. `+` adds two numbers or concatenates two
strings and does not coerce mixed types. Parenthesize any mix of `??` with
`&&` or `||`.

There are no implicit clock variables. Pass time into the runtime explicitly
when a graph or directive needs it.

### Functions

Functions use ordinary call syntax and may be nested:

| Function                  | Description                              |
|---------------------------|------------------------------------------|
| `length(value)`           | Length of an array, object, or string    |
| `contains(container, needle)` | Membership or substring test         |
| `keys(object)`            | Object keys in deterministic lexical order |
| `upper(string)`           | Uppercase string                         |
| `lower(string)`           | Lowercase string                         |
| `json(value)`             | Serialize as compact JSON text           |
| `from_json(string)`       | Parse JSON text                          |
| `type(value)`             | JSON type name                           |
| `exists(path)`            | Whether a context path is present, including explicit null |
| `matches(string, regex)`  | Regular-expression match                 |
| `string(value)`           | Explicit text conversion; structures use compact JSON |
| `number(value)`           | Convert a compatible value to a number   |

Examples: `${json(inputs.messages)}`, `${upper(inputs.name)}`,
`${length(inputs.items ?? [])}`.

### Template Rendering

When the entire template string is exactly `${expression}`, the resolved value
retains its native JSON type, including `null`, boolean, number, string, array,
or object. In surrounding text, strings, numbers, and booleans render directly,
and explicit `null` renders as empty text. Embedded arrays and objects are an
error; use `json(...)` or `string(...)` explicitly. `$${` emits a literal `${`.
Rendering is one pass, so text produced by an expression is never evaluated as
a second template.

### Context Roots

- Directive bodies expose only `inputs`, and direct references must name one
  exact input so unreferenced inputs can still be appended once.
- Graph fields expose `state` and `inputs`; `_execution` and `_run` are present
  only when supplied by the launch context. A declared foreach/fanout variable
  is available in that node's per-item fields.
- `result` is available after an action for that node's `assign` and conditional
  `next`. It is not a store of prior-node results; persist values needed later
  into `state`.
- Hook roots are event-specific and are validated while the hook is compiled.

### Where Used
- Graph YAML: node actions, params, assign, foreach `over`, facets, output, and
  scalar edge conditions. Template-bearing values use `${expression}`;
  condition fields use a bare scalar expression such as `state.ready && result.ok`.
- Directive prompts: `${inputs.target}` in the body text
- Hook definitions: bare scalar conditions and `${expression}` action templates

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

## Execution Order

When all systems could apply to the same data, they run in this order:

1. **Daemon compile time:** System 2 (env `${VAR}`) → System 1 (`{token}`)
2. **Route dispatch time:** System 3 (`${path.X}`)
3. **Runtime subprocess:** System 4 (`rye-expr/1` `${expression}`)
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
| `${expression}`  | System 4     | Runtime bodies     |
| `{key}`        | System 5        | Provider templates |
