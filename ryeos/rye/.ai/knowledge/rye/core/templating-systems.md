<!-- rye:signed:2026-02-23T05:24:41Z:f8c5799da85fef9b68669bade217bb5f9c17240c3302b37e8a56889868dc2c35:OAHtwNqJVgpyT-Ck7WA3Dl7zfYQBcr-CXXg4HOoU-OMb_nIhXgNBVRWFRA8vpcAgjActV19rWhvWhCO_wNH3AQ==:9fbfabe975fa5a7f -->

```yaml
name: templating-systems
title: Templating and Interpolation Systems
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - interpolation
  - templating
  - env-vars
  - runtime
  - hooks
  - graphs
  - parameter-substitution
  - environment-variables
  - tool-path
  - params-json
  - project-path
  - variable-expansion
references:
  - executor-chain
  - input-interpolation
```

# Templating and Interpolation Systems

Rye has four distinct templating systems. Each uses a different syntax, runs at a different stage, and operates on different data. This document is the single authoritative reference.

## Overview

| #   | Syntax                       | Regex                        | Resolver                                                  | Runs On                                  | Stage                                  |
| --- | ---------------------------- | ---------------------------- | --------------------------------------------------------- | ---------------------------------------- | -------------------------------------- |
| 1   | `${VAR}` / `${VAR:-default}` | `[A-Z_][A-Z0-9_]*(:-...)?`   | `PrimitiveExecutor`, `SubprocessPrimitive`, `EnvResolver` | Runtime YAML config, command/args        | Tool execution (chain config building) |
| 2   | `{param_name}`               | `\{(\w+)\}` or `\{([^}]+)\}` | `PrimitiveExecutor`, `SubprocessPrimitive`                | Runtime YAML config, command/args        | Tool execution (after env expansion)   |
| 3   | `${dotted.path}`             | `\$\{([^}]+)\}`              | `loaders/interpolation.py`                                | Hook action params, graph node templates | Before dispatch (hooks, graph walker)  |
| 4   | `{input:key}`                | `\{input:(\w+)(...)\}`       | `execute.py._resolve_input_refs()`                        | Directive body, actions, content         | Directive execution                    |

## System 1: Environment Variable Expansion

**Syntax:** `${VAR_NAME}` or `${VAR_NAME:-default_value}`

**Constraint:** Variable names must be uppercase letters, digits, and underscores only (`[A-Z_][A-Z0-9_]*`). This is enforced by regex to prevent collision with System 3's dotted-path syntax.

**Where it runs:**

- `PrimitiveExecutor._template_config()` — Pass 1 on merged execution config
- `SubprocessPrimitive._template_env_vars()` — Stage 1 on command/args/cwd
- `EnvResolver._expand_variables()` — on static env values in `env_config.env`

**Examples in YAML:**

```yaml
env_config:
  env:
    PYTHONUNBUFFERED: "1"
    PROJECT_VENV_PYTHON: "${RYE_PYTHON}"

config:
  command: "${RYE_PYTHON}"
```

**Missing variables:** resolve to `""` (empty string), or to the default if `:-default` is specified.

**Important:** This system will NOT match lowercase or dotted paths. `${state.issues}` passes through untouched.

## System 2: Runtime Parameter Substitution

**Syntax:** `{param_name}`

**Where it runs:**

- `PrimitiveExecutor._template_config()` — Pass 2 on merged execution config (up to 3 iterations until stable)
- `SubprocessPrimitive._template_params()` — Stage 2 on command/args/cwd
- `PrimitiveExecutor._template_string()` — on anchor env_paths

**Available parameters:**

| Parameter                     | Source                | Description                    |
| ----------------------------- | --------------------- | ------------------------------ |
| `{tool_path}`                 | Chain element         | Absolute path to the tool file |
| `{tool_dir}`                  | Chain element         | Directory containing the tool  |
| `{params_json}`               | Serialized parameters | JSON string of tool parameters |
| `{project_path}`              | Executor context      | Project root path              |
| `{anchor_path}`               | Anchor resolution     | Module resolution root         |
| `{runtime_lib}`               | Anchor config         | Runtime library path           |
| `{user_space}`                | Executor context      | User space path                |
| `{system_space}`              | Executor context      | System space path              |
| `{command}`                   | Config merge          | For bash runtime               |
| `{model}`, `{messages}`, etc. | Provider config       | HTTP provider body fields      |

**Missing parameters:** left unchanged in the string (not replaced with empty).

**Type preservation:** When a value is exactly `"{param}"` (the entire string), the original typed value is returned (int, list, dict). When mixed with text like `"prefix-{param}"`, `str()` is used.

**Examples in YAML:**

```yaml
config:
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
```

## System 3: Context Interpolation

**Syntax:** `${dotted.path}`

**Where it runs:**

- `loaders/interpolation.py` — `interpolate()` and `interpolate_action()`
- Called by `safety_harness.py` for hook action params
- Called by the state graph walker for node action params and assign expressions

**Resolution:** Uses `condition_evaluator.resolve_path(context, path)` to traverse nested dicts via dotted paths.

**Context namespaces (for graphs):**

| Namespace | Description                     | Example           |
| --------- | ------------------------------- | ----------------- |
| `state`   | Current graph state             | `${state.issues}` |
| `inputs`  | Graph input parameters          | `${inputs.files}` |
| `result`  | Current node's unwrapped result | `${result.fixes}` |

**Context namespaces (for hooks):**

| Namespace   | Description            | Example           |
| ----------- | ---------------------- | ----------------- |
| `directive` | Current directive name | `${directive}`    |
| `model`     | Current LLM model      | `${model}`        |
| `limits`    | Thread limits dict     | `${limits.turns}` |

**Missing paths:** resolve to `""` (empty string). The walker logs warnings for `assign` expressions that resolve to empty when the template was non-empty.

**Works recursively** on strings, dicts, and lists. Non-string leaves are returned as-is.

**No collision with System 1** because System 1's regex only matches uppercase env var names, while System 3 paths are always lowercase dotted (e.g., `state.issues`).

## System 4: Directive Input References

**Syntax:** `{input:key}`, `{input:key?}`, `{input:key:default}`, `{input:key|default}`

**Where it runs:**

- `execute.py._resolve_input_refs()` and `_interpolate_parsed()`
- During directive execution via `ExecuteTool._run_directive()`

**Operates on:** directive `body`, `content`, `raw`, and `actions` fields.

| Pattern               | Behavior                                 |
| --------------------- | ---------------------------------------- |
| `{input:key}`         | Required — kept as literal if missing    |
| `{input:key?}`        | Optional — empty string if missing       |
| `{input:key:default}` | Fallback — uses default value if missing |

**No collision with System 2** because the `input:` namespace prefix distinguishes them. System 2's regex would match the outer braces, but since `input:key` is never a key in the params dict, it's left unchanged.

See [input-interpolation](input-interpolation) for full details.

## Execution Order

When a tool is executed through the full chain, templating happens in this order:

```
1. Directive input refs ({input:key})     — execute.py, during directive parsing
2. Context interpolation (${state.X})     — interpolation.py, in hooks/graph walker
3. Env var expansion (${VAR})             — PrimitiveExecutor Pass 1
4. Runtime param substitution ({param})   — PrimitiveExecutor Pass 2
5. Env var expansion (${VAR})             — SubprocessPrimitive Stage 1 (redundant but safe)
6. Runtime param substitution ({param})   — SubprocessPrimitive Stage 2 (redundant but safe)
```

Steps 5-6 are redundant with 3-4 but harmless — they catch any templates that survived the PrimitiveExecutor pass (e.g., if a parameter value itself contains `{project_path}`).

## Collision Prevention Rules

1. **System 1 vs System 3:** Prevented by regex constraint. System 1 only matches `[A-Z_][A-Z0-9_]*`. Context paths like `${state.issues}` contain lowercase and dots, so they pass through env expansion untouched.

2. **System 2 vs System 4:** Prevented by namespace. `{input:key}` contains `:` which is not a valid param name in System 2's typical usage. System 2 leaves unknown params unchanged.

3. **System 2 vs System 3:** No overlap. `${...}` (dollar prefix) is System 1/3. `{...}` (bare braces) is System 2/4.

## Conventions

- **Env var names:** Always `UPPER_SNAKE_CASE`. Never use dots or lowercase in env var names.
- **Runtime params:** Always `lower_snake_case`. The set is fixed by the runtime/executor, not user-extensible.
- **Context paths:** Always `namespace.field` with lowercase dotted paths.
- **Directive inputs:** Always `{input:snake_case}` with the `input:` prefix.
