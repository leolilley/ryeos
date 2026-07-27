<!-- ryeos:signed:2026-07-27T09:03:55Z:6cc9605bffb1a595da9b387839309c8921ad635d3a46e92ae31c1c87d56f9a12:AbukfpUVNkKUtggUyREsw1XjvxyelozkyhQmp1NpTISwdZ8KSNZhXFAi2E55r+hgtCM0RgTKpN2PTbYoOp6pCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core
tags: [reference, templating, interpolation, substitution]
version: "1.0.0"
description: >
  The interpolation/template surfaces in Rye OS — where each runs, which
  use rye-expr/1, and what context each surface exposes.
---

# Templating and Interpolation

Rye OS has four template/interpolation surfaces. Runtime subprocess fields and
graph/directive bodies share the bounded `rye-expr/1` language, but expose
different context roots. HTTP route captures and provider adapters retain
narrow, surface-specific substitution contracts.

## Where Each System Runs

```
┌─────────────────────────────────────────────────────────────┐
│  RUST DAEMON (ryeosd / ryeos-engine)                        │
│                                                             │
│  Surface 1: rye-expr/1 ${expression}                       │
│             — tool command/args/input/cwd/env               │
│  Surface 2: ${path.X} — HTTP route source_config            │
├─────────────────────────────────────────────────────────────┤
│                                 │ fork + exec               │
│                                 ▼                           │
│  RUST RUNTIME SUBPROCESS                                    │
│  (ryeos-graph-runtime, ryeos-directive-runtime)             │
│                                                             │
│  Surface 3: rye-expr/1 ${expression} — runtime bodies      │
│  Surface 4: {key} exact-match — provider API templates     │
└─────────────────────────────────────────────────────────────┘
```

---

## Surface 1: Runtime Subprocess Expressions

**Runs in:** Rust daemon (`ryeosd`), during the `compile_with_handlers` pipeline.
**Syntax:** `${expression}` using the same bounded `rye-expr/1` compiler and
evaluator as graph/directive runtime bodies.
**Unknown roots or invalid expressions:** Hard error at plan-build time.

### Available Context Roots

| Expression root      | Value                                        |
|----------------------|----------------------------------------------|
| `${tool_path}`       | Absolute path to the tool source file        |
| `${tool_dir}`        | Parent directory of the tool source file     |
| `${tool_parent}`     | Grandparent of the tool source file          |
| `${project_path}`    | Absolute path to the project root            |
| `${params_json}`     | Full parameters as JSON string               |
| `${interpreter}`     | Resolved Python binary (from env config)     |
| `${runtime_dir}`     | Current chain element's directory            |

### Where Used

In tool YAML `config` blocks:
```yaml
config:
  command: "${interpreter}"
  args: ["${tool_path}", "--project-path", "${project_path}"]
  input_data: "${params_json}"
  cwd: "${tool_dir}"
```

The Python runtimes no longer set `PYTHONPATH`; they derive
bundle-local import roots from `${tool_path}` and prepend them to
`sys.path` inside the runtime launcher.

The same expression pass renders `env_config.env` and
`env_config.env_paths`. In those fields only, uppercase expression roots such
as `${PATH}` request a host environment value. The name must be present in
`RYEOS_TOOL_ENV_PASSTHROUGH`; reserved `RYEOS_*` and `RYEOSD_*` names are
rejected. Vault-managed secrets continue to use `required_secrets`, not host
environment passthrough.

```yaml
env_config:
  env:
    PATH: "${PATH}"                         # allowlisted host value
    PYTHONUNBUFFERED: "1"                   # literal
    PROJECT_VENV_PYTHON: "${interpreter}"   # runtime context
  env_paths:
    PATH:
      prepend: ["${runtime_dir}/bin"]
```

Rendering is a single pass. `$${` emits a literal `${`, and ordinary braces are
not template syntax. For example, an embedded Python f-string containing
`{tool_path}` remains unchanged; `$${tool_path}` emits the literal text
`${tool_path}`.

---

## Surface 2: Route `source_config` Path Interpolation

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

## Surface 3: Rust Graph/Directive Interpolation

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

## Surface 4: Provider Template Substitution

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

When multiple surfaces participate in one execution, they run in this order:

1. **Daemon compile time:** Surface 1 (`rye-expr/1` runtime subprocess fields)
2. **Route dispatch time:** Surface 2 (`${path.X}`)
3. **Runtime subprocess:** Surface 3 (`rye-expr/1` graph/directive bodies)
4. **Provider formatting:** Surface 4 (`{key}` exact-match)

Each template value is rendered once by the surface that owns it. Generated
text is never reparsed as another template in the same surface.

## Collision Prevention

The syntaxes are designed to be distinguishable:

| Syntax            | Surface                       | Context                    |
|-------------------|-------------------------------|----------------------------|
| `${expression}`   | Runtime subprocess rye-expr/1 | Tool config and env fields |
| `${path.X}`       | Route interpolation           | Route configs only         |
| `${expression}`   | Runtime body rye-expr/1       | Graph/directive bodies     |
| `{key}`           | Provider substitution         | Provider templates         |

The two rye-expr/1 surfaces intentionally share syntax and semantics while
offering only the roots authorized for their respective execution phase.
