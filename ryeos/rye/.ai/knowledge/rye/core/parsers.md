<!-- rye:signed:2026-02-21T05:56:40Z:3568ab0e3235ebbe847a87552bfd730ee4244150c369da14c0794383018eeb3a:KnY8yGBmK-qVNB3ORDi7h8MmPFp8CihHOlrFOAGrR6KErwJq4r1yzTHbmdEGU4fddEgEiO46oGwT_e4Ts9ylBw==:9fbfabe975fa5a7f -->

```yaml
id: parsers
title: Parsers
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - parsers
  - markdown
  - xml
  - yaml
  - ast
references:
  - "docs/standard-library/tools/core.md"
```

# Parsers

The 4 parsers that extract structured metadata from item files. Located at `.ai/tools/rye/core/parsers/`.

## Parser ↔ Item Type Mapping

| Parser                 | Item Type  | File Extensions       |
| ---------------------- | ---------- | --------------------- |
| `markdown_xml`         | Directive  | `.md`                 |
| `markdown_frontmatter` | Knowledge  | `.md`                 |
| `python_ast`           | Tool       | `.py`                 |
| `yaml`                 | Tool       | `.yaml`, `.yml`       |

## `markdown_xml` — Directive Parser

Parses Markdown files containing embedded XML metadata in fenced code blocks.

### Input

A `.md` file with this structure:

```
Line 1:  <!-- rye:signed:TIMESTAMP:HASH:SIG:FP -->
         # Title
         Description text
         ```xml
         <directive name="..." version="...">
           <metadata>...</metadata>
           <inputs>...</inputs>
           <outputs>...</outputs>
         </directive>
         ```
         <process>
           <step name="...">...</step>
         </process>
```

### Extracts

| Field         | Source                       | Type     |
| ------------- | ---------------------------- | -------- |
| `name`        | `<directive name="...">`     | string   |
| `version`     | `<directive version="...">`  | string   |
| `description` | `<description>` element      | string   |
| `category`    | `<category>` element         | string   |
| `author`      | `<author>` element           | string   |
| `model`       | `<model>` attributes         | dict     |
| `limits`      | `<limits>` attributes        | dict     |
| `permissions` | `<permissions>` tree         | list     |
| `hooks`       | `<hooks>` elements           | list     |
| `inputs`      | `<input>` elements           | list     |
| `outputs`     | `<output>` elements          | list     |
| `body`        | Everything after XML fence   | string   |
| `actions`     | Parsed from process steps    | list     |
| `content`     | Rendered directive content   | string   |
| `raw`         | Full file content            | string   |

### Key Behavior

- XML fence is parsed for infrastructure metadata (limits, permissions, model)
- Process steps after the fence are natural language for the LLM
- Signature comment on line 1 is extracted separately
- Permissions are converted to capability strings: `{tag: "cap", content: "rye.<primary>.<type>.<pattern>"}`

## `markdown_frontmatter` — Knowledge Parser

Parses Markdown files with YAML metadata in ` ```yaml ` code fences (matching how `markdown_xml` uses ` ```xml ` fences for directives). Also handles pure YAML files.

### Input

````markdown
```yaml
id: my-knowledge
title: My Knowledge Entry
entry_type: reference
category: rye/core
version: "1.0.0"
tags:
  - tag1
  - tag2
```

# Content starts here
````

### Extracts

| Field        | Source                        | Type        |
| ------------ | ----------------------------- | ----------- |
| `id`         | YAML `id`                     | string      |
| `title`      | YAML `title`                  | string      |
| `entry_type` | YAML `entry_type`             | string      |
| `category`   | YAML `category`               | string      |
| `version`    | YAML `version`                | string      |
| `author`     | YAML `author`                 | string      |
| `tags`       | YAML `tags`                   | list[str]   |
| `references` | YAML `references`             | list[str]   |
| `body`       | Everything after closing ` ``` ` | string   |

### Key Behavior

- Uses `re.search(r"^```yaml\s*$", ...)` to find the fence (mirrors `_extract_xml_block`)
- YAML inside the fence is parsed with `yaml.safe_load` — full YAML support
- Content below the closing ` ``` ` is the knowledge body
- Pure `.yaml`/`.yml` files: entire content parsed as YAML metadata (signature stripped first)
- `rye_execute(item_type="knowledge")` returns only body content
- `rye_load(item_type="knowledge")` returns full file with metadata

## `python_ast` — Python Tool Parser

Parses Python files using AST to extract dunder metadata and `CONFIG_SCHEMA`.

### Input

```python
# rye:signed:TIMESTAMP:HASH:SIG:FP

"""Tool docstring."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/bash"
__tool_description__ = "Execute shell commands"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "command": {"type": "string", "description": "Command to run"}
    }
}
```

### Extracts

| Field              | Source                    | Type   |
| ------------------ | ------------------------- | ------ |
| `version`          | `__version__`             | string |
| `tool_type`        | `__tool_type__`           | string |
| `executor_id`      | `__executor_id__`         | string |
| `category`         | `__category__`            | string |
| `tool_description` | `__tool_description__`    | string |
| `config_schema`    | `CONFIG_SCHEMA`           | dict   |

### Key Behavior

- Uses Python `ast` module — does not execute the file
- Extracts only module-level assignments with constant values
- `CONFIG_SCHEMA` is a JSON Schema dict defining tool parameters
- `__executor_id__` is critical — points to the next chain element

## `yaml` — YAML Tool Parser

Parses YAML files for runtime configs and tool definitions.

### Input

```yaml
tool_id: rye/core/runtimes/python_script_runtime
tool_type: runtime
executor_id: rye/core/primitives/subprocess

env_config:
  interpreter:
    type: venv_python
    venv_path: .venv

config:
  command: "${RYE_PYTHON}"
  args: ["{tool_path}", "--params", "{params_json}"]
  timeout: 300

parameters:
  - name: command
    type: string
    required: true
```

### Extracts

| Field         | Source            | Type   |
| ------------- | ----------------- | ------ |
| `tool_id`     | Top-level key     | string |
| `tool_type`   | Top-level key     | string |
| `executor_id` | Top-level key     | string |
| `env_config`  | Top-level key     | dict   |
| `config`      | Top-level key     | dict   |
| `parameters`  | Top-level key     | list   |
| `anchor`      | Top-level key     | dict   |
| `verify_deps` | Top-level key     | dict   |

### Key Behavior

- All top-level keys are extracted as metadata
- Used for runtimes (`.yaml`) and primitive configs
- `executor_id` points to next chain element (or `None` for primitives)
- `env_config` drives interpreter and environment resolution

## Extractors vs Parsers

Parsers extract raw metadata. **Extractors** (YAML configs at `.ai/tools/rye/core/extractors/`) define:
- Which parser to use per item type
- Which fields to index for search
- Validation rules for required metadata
- How parameters are extracted and mapped

| Extractor Config                     | Item Type  |
| ------------------------------------ | ---------- |
| `directive/directive_extractor.yaml` | Directives |
| `tool/tool_extractor.yaml`           | Tools      |
| `knowledge/knowledge_extractor.yaml` | Knowledge  |
