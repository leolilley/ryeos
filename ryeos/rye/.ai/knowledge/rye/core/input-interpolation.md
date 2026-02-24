<!-- rye:signed:2026-02-24T05:50:18Z:300b35599e534a5faa82e920a800dcb8bc95648d1e5897b4c6e310ead1e5d10c:xnhmSQWU4R_34B8HfF17UvSI41P5PXDrhQzLgCvMp1AFKCO8dvS9UTznKCep7wDXvzZ0SFEjeH4d3zfeAypCDw==:9fbfabe975fa5a7f -->
```yaml
name: input-interpolation
title: Input Interpolation
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - interpolation
  - inputs
  - directives
  - templates
  - placeholders
  - parameters
  - input-resolution
  - directive-inputs
references:
  - templating-systems
  - "docs/authoring/directives.md"
  - "docs/tools-reference/execute.md"
```

# Input Interpolation

How `{input:name}` placeholders are resolved in directives during execution.

## Syntax

| Pattern               | Behavior                                                |
| --------------------- | ------------------------------------------------------- |
| `{input:key}`         | **Required** — kept as literal `{input:key}` if missing |
| `{input:key?}`        | **Optional** — replaced with empty string if missing    |
| `{input:key:default}` | **Fallback** — uses `default` value if key missing      |
| `{input:key\|default}` | **Fallback** — uses `default` value if key missing (pipe syntax) |

## Where Interpolation Runs

When `rye_execute` processes a directive, `_interpolate_parsed()` replaces placeholders during the validation step before thread spawning:

| Field     | Description                                                   |
| --------- | ------------------------------------------------------------- |
| `body`    | The full directive body text (everything after the XML fence) |
| `content` | The rendered content of the directive                         |
| `raw`     | The raw file content                                          |
| `actions` | All action elements extracted from process steps              |

Every string field in these locations is scanned for `{input:...}` patterns.

## Input Declaration

Inputs are declared in the directive's XML metadata fence:

```xml
<inputs>
  <input name="name" type="string" required="true">
    Description of this input
  </input>
  <input name="timeout" type="integer" required="false" default="120">
    Timeout in seconds
  </input>
</inputs>
```

### Input Attributes

| Attribute  | Required | Values                                   | Effect                                  |
| ---------- | -------- | ---------------------------------------- | --------------------------------------- |
| `name`     | yes      | snake_case string                        | The key used in `{input:name}`          |
| `type`     | yes      | `string`, `integer`, `boolean`, `object` | Type hint (informational)               |
| `required` | yes      | `true`, `false`                          | Whether execution fails without it      |
| `default`  | no       | any string                               | Applied before interpolation if missing |

## Execution Flow

1. **Defaults applied** — declared inputs with `default` values are merged into the parameters dict
2. **Required validation** — required inputs without values produce an error with the full `declared_inputs` list
3. **Interpolation** — `_interpolate_parsed()` replaces all `{input:...}` placeholders in body, content, raw, and actions

## Examples in Process Steps

### XML format

```xml
<process>
  <step name="check_duplicates">
    Search for directives similar to {input:name}.
    <search item_type="directive" query="{input:name}" />
  </step>

  <step name="write_file">
    Write to .ai/directives/{input:category}/{input:name}.md
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value=".ai/directives/{input:category}/{input:name}.md" />
    </execute>
  </step>
</process>
```

### Backtick format

```markdown
**Check duplicates**
`rye_search(item_type="directive", query="{input:name}")`

**Write file**
`rye_execute(item_type="tool", item_id="rye/file-system/write",
    parameters={"path": ".ai/directives/{input:category}/{input:name}.md"})`
```

## Optional and Fallback Patterns

```xml
<step name="greet">
  Hello {input:user_name}, welcome to {input:project_name?}!
  Your role is {input:role:developer}.
</step>
```

- `{input:user_name}` — required; if missing, literal `{input:user_name}` appears in output
- `{input:project_name?}` — optional; replaced with `""` if not provided
- `{input:role:developer}` — fallback; uses `"developer"` if not provided

## Error on Missing Required Inputs

When required inputs are missing, execution returns an error response:

```json
{
  "status": "error",
  "error": "Missing required inputs: name, category",
  "item_id": "rye/core/create_directive",
  "declared_inputs": [
    { "name": "name", "type": "string", "required": true },
    { "name": "category", "type": "string", "required": true }
  ]
}
```

## Outputs → Returns Transformation

Directive `<outputs>` are transformed into `<returns>` and appended to the prompt for threaded execution. The LLM never sees raw `<outputs>` XML — it sees:

```xml
<returns>
  <output name="directive_path">Path to the created file</output>
  <output name="signed">Whether signing succeeded</output>
</returns>
```

Output names must be consistent between the directive declaration and what the parent thread expects.
