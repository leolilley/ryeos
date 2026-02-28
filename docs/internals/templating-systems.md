```yaml
id: templating-systems
title: "Templating and Interpolation Systems"
description: The four distinct templating systems in rye, their syntax, execution stages, and collision prevention
category: internals
tags: [templating, interpolation, env-vars, runtime, hooks, graphs]
version: "1.0.0"
```

# Templating and Interpolation Systems

Rye has four templating systems. Each uses a different syntax, runs at a different stage, and operates on different data. They must not collide.

## The Four Systems

| #   | Syntax           | Purpose               | Resolver                                                  |
| --- | ---------------- | --------------------- | --------------------------------------------------------- |
| 1   | `${VAR}`         | Environment variables | `PrimitiveExecutor`, `SubprocessPrimitive`, `EnvResolver` |
| 2   | `{param}`        | Runtime parameters    | `PrimitiveExecutor`, `SubprocessPrimitive`                |
| 3   | `${dotted.path}` | Context interpolation | `loaders/interpolation.py`                                |
| 4   | `{input:key}`    | Directive inputs      | `execute.py`                                              |

## System 1: Environment Variable Expansion

**Syntax:** `${VAR_NAME}` or `${VAR_NAME:-default_value}`

**Regex:** `\$\{([A-Z_][A-Z0-9_]*(?::-[^}]*)?)\}`

Variable names are constrained to uppercase letters, digits, and underscores. This is enforced by regex to prevent collision with System 3's dotted-path syntax — `${state.issues}` is never consumed as an env var.

**Where it runs:**

1. `EnvResolver._expand_variables()` — resolves `${VAR}` in static env values from `env_config.env`
2. `PrimitiveExecutor._template_config()` Pass 1 — resolves `${VAR}` across the merged execution config
3. `SubprocessPrimitive._template_env_vars()` Stage 1 — resolves `${VAR}` in command/args/cwd

**Missing variables:** resolve to `""`, or to the default if `:-default` is specified.

**Example:**

```yaml
config:
  command: "${RYE_PYTHON}" # → /path/to/.venv/bin/python3
env_config:
  env:
    PROJECT_VENV_PYTHON: "${RYE_PYTHON}"
```

## System 2: Runtime Parameter Substitution

**Syntax:** `{param_name}`

**Regex:** `\{(\w+)\}` (PrimitiveExecutor) or `\{([^}]+)\}` (SubprocessPrimitive)

**Where it runs:**

1. `PrimitiveExecutor._template_config()` Pass 2 — substitutes `{param}` in merged config, iterates up to 3 times until stable
2. `SubprocessPrimitive._template_params()` Stage 2 — substitutes `{param}` in command/args/cwd
3. `PrimitiveExecutor._template_string()` — substitutes `{var}` in anchor env_paths

**Available parameters:**

| Parameter                     | Description                     |
| ----------------------------- | ------------------------------- |
| `{tool_path}`                 | Absolute path to the tool file  |
| `{tool_dir}`                  | Directory containing the tool   |
| `{tool_parent}`               | Parent directory of the tool    |
| `{params_json}`               | JSON-serialized tool parameters (piped via `input_data`/stdin) |
| `{project_path}`              | Project root path               |
| `{anchor_path}`               | Module resolution root          |
| `{runtime_lib}`               | Runtime library path            |
| `{user_space}`                | User space path                 |
| `{system_space}`              | System space path               |
| `{model}`, `{messages}`, etc. | HTTP provider body fields       |

**Missing parameters:** left unchanged in the string (not replaced with empty).

**Type preservation:** When a value is exactly `"{param}"`, the original typed value is returned (int, list, dict). Mixed text like `"prefix-{param}"` uses `str()`.

**Example:**

```yaml
config:
  args:
    - "{tool_path}" # → /path/to/my_tool.py
    - "--project-path"
    - "{project_path}" # → /home/user/my-project
  input_data: "{params_json}" # → '{"files": ["a.py"]}' (piped via stdin)
```

## System 3: Context Interpolation

**Syntax:** `${dotted.path}`

**Regex:** `\$\{([^}]+)\}`

This is the same dollar-brace syntax as System 1, but System 1's tightened regex (`[A-Z_][A-Z0-9_]*`) means dotted lowercase paths like `${state.issues}` are never matched by the env var resolver.

**Where it runs:**

- `loaders/interpolation.py` — `interpolate()` and `interpolate_action()`
- Called by `safety_harness.py` for hook action params (before dispatch)
- Called by the state graph walker for node action params and assign expressions (before dispatch)

**Resolution:** `condition_evaluator.resolve_path(context, path)` traverses nested dicts via dotted paths.

**Namespaces in graphs:**

| Namespace | Example           | Description                     |
| --------- | ----------------- | ------------------------------- |
| `state`   | `${state.issues}` | Current graph state             |
| `inputs`  | `${inputs.files}` | Graph input parameters          |
| `result`  | `${result.fixes}` | Current node's unwrapped result |

**Namespaces in hooks:**

| Namespace   | Example           | Description            |
| ----------- | ----------------- | ---------------------- |
| `directive` | `${directive}`    | Current directive name |
| `model`     | `${model}`        | Current LLM model      |
| `limits`    | `${limits.turns}` | Thread limits          |

**Missing paths:** resolve to `""` (empty string). The walker logs warnings for non-empty templates that resolve to empty.

**Type preservation:** When a template is a single whole expression (`"${path}"` with no surrounding text), the raw resolved value is returned without string conversion. This means `assign: { count: "${result.stdout}" }` where `result.stdout` is an integer preserves the integer type. Mixed templates like `"Found ${state.count} items"` use string conversion. This is critical for graph edge conditions that use numeric comparisons (`op: gt, value: 0`).

## System 4: Directive Input References

**Syntax:** `{input:key}`, `{input:key?}`, `{input:key:default}`, `{input:key|default}`

**Regex:** `\{input:(\w+)(\?|[:|][^}]*)?\}`

**Where it runs:**

- `execute.py._resolve_input_refs()` and `_interpolate_parsed()`
- During directive execution via `directive_parser.parse_and_validate_directive()`

**Operates on:** directive `body`, `content`, `raw`, and `actions` fields.

| Pattern               | Behavior                              |
| --------------------- | ------------------------------------- |
| `{input:key}`         | Required — kept as literal if missing |
| `{input:key?}`        | Optional — empty string if missing    |
| `{input:key:default}` | Fallback — uses default value         |

No collision with System 2 because the `input:` prefix is never a valid runtime parameter name.

## Execution Order

When a tool executes through the full chain, templating runs in this order:

```
1. {input:key}        — execute.py during directive parsing
2. ${dotted.path}     — interpolation.py in hooks/graph walker (before dispatch)
   ─── dispatch boundary ───
3. ${VAR}             — PrimitiveExecutor._template_config() Pass 1
4. {param}            — PrimitiveExecutor._template_config() Pass 2
5. ${VAR}             — SubprocessPrimitive._template_env_vars() Stage 1
6. {param}            — SubprocessPrimitive._template_params() Stage 2
```

Steps 5-6 are redundant with 3-4 but harmless — they catch templates that survived the PrimitiveExecutor pass.

The dispatch boundary is critical: Systems 1-2 (pre-dispatch) operate on the _values_ that will be passed to `rye_execute`. Systems 3-6 (post-dispatch) operate on the _config_ that controls how the tool runs. These are different data flowing through different code paths.

## Collision Prevention

| Pair          | Risk                    | Prevention                                                                                            |
| ------------- | ----------------------- | ----------------------------------------------------------------------------------------------------- |
| System 1 vs 3 | Same `${...}` delimiter | System 1 regex rejects lowercase and dots. `${state.issues}` passes through env expansion untouched.  |
| System 2 vs 4 | Both use `{...}`        | `{input:key}` contains `:` which is not a valid param name. System 2 leaves unknown params unchanged. |
| System 2 vs 3 | Different prefixes      | `${...}` (dollar) is System 1/3. `{...}` (bare) is System 2/4. No overlap.                            |

## Conventions

- **Env vars:** `UPPER_SNAKE_CASE` only. Never dots or lowercase.
- **Runtime params:** `lower_snake_case`. Fixed set defined by the runtime/executor.
- **Context paths:** `namespace.field` with lowercase dotted paths.
- **Directive inputs:** `{input:snake_case}` with the `input:` prefix.

## Implementation Files

| System | Primary Implementation                                                                                                                                                           |
| ------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1      | `rye/executor/primitive_executor.py` (`_template_config` Pass 1), `lillux/primitives/subprocess.py` (`_template_env_vars`), `lillux/runtime/env_resolver.py` (`_expand_variables`) |
| 2      | `rye/executor/primitive_executor.py` (`_template_config` Pass 2, `_template_string`), `lillux/primitives/subprocess.py` (`_template_params`)                                      |
| 3      | `rye/.ai/tools/rye/agent/threads/loaders/interpolation.py`, `rye/.ai/tools/rye/agent/threads/loaders/condition_evaluator.py`                                                     |
| 4      | `rye/tools/execute.py` (`_resolve_input_refs`, `_interpolate_parsed`)                                                                                                            |
