# Injection Hardening

RYE tools are configured through template substitution. The `PrimitiveExecutor._template_config()` method ([`rye/rye/executor/primitive_executor.py`](rye/rye/executor/primitive_executor.py)) handles this with explicit shell injection prevention.

## Template Substitution

`_template_config()` performs two substitution passes:

### Pass 1: Environment Variables (`${VAR}`)

Environment variable references are resolved from the execution environment. All values containing shell metacharacters are escaped with `shlex.quote()`.

```python
# Input:  command: "echo ${USER_INPUT}"
# If USER_INPUT = "$(rm -rf /)"
# Output: command: "echo '$(rm -rf /)'"
```

Supports default values with `${VAR:-default}` syntax.

### Pass 2: Config Parameters (`{param}`)

Config values reference other config values. This pass runs up to 3 iterations until stable (no further substitutions possible).

When a value is exactly `{param}` (the entire string is one placeholder), the original typed value is preserved — an integer stays an integer, a list stays a list. Mixed strings like `"prefix-{param}"` use string conversion.

## Shell Escaping

The `escape_shell_value()` function applies `shlex.quote()` when a value contains any of these shell metacharacters:

```
$ ` ; | & < > ( ) { } [ ] \
```

Safe values (no metacharacters) pass through unquoted.

Examples:

| Input          | Output          | Reason               |
| -------------- | --------------- | -------------------- |
| `hello`        | `hello`         | No metacharacters    |
| `$(rm -rf /)`  | `'$(rm -rf /)'` | `$` and `(` detected |
| `` `whoami` `` | ``'`whoami`'``  | Backticks detected   |
| `foo; bar`     | `'foo; bar'`    | Semicolon detected   |
| `normal-text`  | `normal-text`   | No metacharacters    |

`shlex.quote()` wraps values in single quotes and escapes any embedded single quotes, preventing shell interpretation.

## Unresolved Placeholder Stripping

Body dict values that are unresolved single placeholders (e.g., `{optional_field}`) are stripped from the request body before execution:

```python
# Before:  {"name": "test", "optional": "{unset_param}"}
# After:   {"name": "test"}
```

This prevents leaking template syntax into API calls or command arguments.

## Substitution Order

The two-pass design is intentional:

1. Environment variables are substituted first with escaping applied
2. Config parameters are substituted second without re-escaping

This prevents double-escaping — a config value referencing an already-escaped env var won't be escaped again.
