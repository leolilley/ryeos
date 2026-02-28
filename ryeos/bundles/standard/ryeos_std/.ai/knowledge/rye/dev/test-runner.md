<!-- rye:signed:2026-02-27T23:55:35Z:079a137dbf9311a7d778702431e5ebe9b5039e1160fb68113fb252dcbe62e132:jF3a3hSFxUNdsTMLKYsNcI8KDhtcm5RxA9uYR5eVxK6E3pKa0KZE3YIwGfEQoKbqaGwG4b5DmQWB_EzpTjZnBw==:4b987fd4e40303ac -->
```yaml
name: test-runner
title: "Tool Test Runner"
description: Execute .test.yaml specs against Rye tools, evaluating assertions on results
entry_type: reference
category: rye/dev
version: "1.0.0"
author: rye-os
created_at: 2026-02-28T00:00:00Z
tags:
  - testing
  - test-runner
  - assertions
  - dev-tools
references:
  - state-graph-walker
  - "docs/orchestration/state-graphs.md"
```

# Tool Test Runner

The test runner (`rye/dev/test_runner.py`) executes `.test.yaml` specs against Rye tools and evaluates assertions on results. Invocable via `rye execute tool rye/dev/test-runner`.

## Test Spec Format

Test specs live at `.ai/tests/**/*.test.yaml`:

```yaml
tool: my-project/scrapers/chart-discovery
tests:
  - name: "discovers games above threshold"
    params:
      min_ccu: 10000
      max_results: 5
    assert:
      success: true
      "result.total_found >= 1": true

  - name: "empty for impossible CCU"
    params:
      min_ccu: 999999999
    assert:
      success: true
      "result.total_found": 0
    tags: [integration]
```

### Fields

| Field | Type | Description |
| --- | --- | --- |
| `tool` | string | Tool ID to test (required) |
| `tests` | list | List of test cases (required) |
| `tests[].name` | string | Human-readable test name |
| `tests[].params` | dict | Parameters passed to the tool |
| `tests[].assert` | dict | Assertions to evaluate on the result |
| `tests[].tags` | list | Tags for filtering (e.g., `[integration, slow]`) |

## Assertion DSL

Two forms, both using the same operators as `condition_evaluator`:

### Simple Form — path equals expected value

```yaml
assert:
  success: true
  exit_code: 0
  stdout: "hello"
```

Resolves `path` via dotted access and compares with `eq`.

### Expression Form — path + operator + expected, value is expected boolean outcome

```yaml
assert:
  "result.total_found >= 1": true
  "error contains timeout": false
  "output regex ^OK": true
  "metadata exists null": true
```

Operators: `==`, `!=`, `>`, `>=`, `<`, `<=`, `contains`, `regex`, `exists`, `in`.

The YAML value (`true`/`false`) is the expected boolean outcome of the expression. This allows negation — `"error exists null": false` asserts that `error` is `None`.

### Assertion Document

Assertions run against a document built from the `ExecuteTool` result envelope:

| Key | Source |
| --- | --- |
| `success` | `true` unless envelope status is `"error"` or inner `success` is `false` |
| `error` | Error message from envelope or inner data |
| `duration_ms` | Execution time from envelope |
| _(tool data keys)_ | Inner `data` dict fields merged to top level (e.g., `stdout`, `exit_code`) |

This means `${result.stdout}` from graph `assign` and `stdout` in test assertions reference the same value.

## Invocation

```python
rye_execute(
    item_type="tool",
    item_id="rye/dev/test-runner",
    parameters={
        "tool": "my-project/scrapers/chart-discovery",
    }
)
```

### Parameters

| Parameter | Type | Description |
| --- | --- | --- |
| `tool` | string | Tool ID filter — only run specs for this tool |
| `spec` | string | Path to a specific `.test.yaml` (relative to project root) |
| `include_tags` | string | Comma-separated tags — only run tests with these tags |
| `exclude_tags` | string | Comma-separated tags — skip tests with these tags |
| `validate_only` | bool | Validate spec structure without executing (default: false) |

### Tag Filtering

```python
# Run only unit tests
rye_execute(item_type="tool", item_id="rye/dev/test-runner",
            parameters={"tool": "my/tool", "include_tags": "unit"})

# Skip integration tests
rye_execute(item_type="tool", item_id="rye/dev/test-runner",
            parameters={"tool": "my/tool", "exclude_tags": "integration,slow"})
```

## Output

### stdout — Structured JSON

```json
{
  "success": true,
  "tool": "my-project/scrapers/chart-discovery",
  "spec_path": ".ai/tests/scrapers/chart-discovery.test.yaml",
  "summary": {
    "total": 2,
    "passed": 2,
    "failed": 0,
    "skipped": 0,
    "duration_ms": 450.2
  },
  "results": [
    {
      "name": "discovers games above threshold",
      "tags": [],
      "passed": true,
      "duration_ms": 320.1,
      "assertions": [
        {"expr": "success == True", "passed": true, "actual": true, "expected": true, "op": "eq"},
        {"expr": "result.total_found >= 1", "passed": true, "actual": 3, "expected": 1, "op": "gte"}
      ]
    }
  ]
}
```

### stderr — Streaming Progress

```
[test] my-project/scrapers/chart-discovery (chart-discovery.test.yaml)
[test] 2 test(s) to run
  ✓ discovers games above threshold (320ms)
  ✗ empty for impossible CCU (130ms) — result.total_found == 0 — got 1
[test] ✗ 1/2 passed (450ms)
```

Suppress with `RYE_TEST_QUIET=1`.

## Implementation

| File | Purpose |
| --- | --- |
| `.ai/tools/rye/dev/test_runner.py` | Test runner tool (standard bundle) |
| `.ai/tests/**/*.test.yaml` | Test spec files (per-project) |
