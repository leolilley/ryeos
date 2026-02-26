<!-- rye:signed:2026-02-23T05:24:41Z:c476466c90f3feed7ab499a851036ebea809e239061f3280f006ea382b7b3e16:SfJ2rHaZOXY5-aISssYdKdaBo4EimUQeOkY_UqJpmKqmVsgc7UVeNvUOQQlCdYz6iJIIrQ-Yt8elCEd2_y2uBg==:9fbfabe975fa5a7f -->

```yaml
name: parsers
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
  - python-ast
  - javascript
  - typescript
  - metadata-extraction
  - frontmatter
  - tool-parsing
  - directive-parsing
  - knowledge-parsing
references:
  - "docs/standard-library/tools/core.md"
```

# Parsers

The 5 parsers that extract structured metadata from item files. Located at `.ai/tools/rye/core/parsers/` in language-specific subdirectories.

```
parsers/
├── javascript/
│   └── javascript.py     # JS/TS tool metadata
├── markdown/
│   ├── frontmatter.py    # Knowledge entry metadata
│   └── xml.py            # Directive metadata
├── python/
│   └── ast.py            # Python tool metadata
└── yaml/
    └── yaml.py           # YAML tool metadata
```

## Parser ↔ Item Type Mapping

| Parser                    | Item Type  | File Extensions                    |
| ------------------------- | ---------- | ---------------------------------- |
| `markdown/xml`            | Directive  | `.md`                              |
| `markdown/frontmatter`    | Knowledge  | `.md`                              |
| `python/ast`              | Tool       | `.py`                              |
| `yaml/yaml`               | Tool       | `.yaml`, `.yml`                    |
| `javascript/javascript`   | Tool       | `.js`, `.ts`, `.mjs`, `.cjs`       |

### Data-Driven Dispatch

The `tool_extractor.yaml` config now includes a `parsers:` field mapping file extensions to parser names. `ParserRouter` (via `get_parsers_map()` from `extensions.py`) uses this map to dispatch metadata extraction — no hardcoded suffix checks in `PrimitiveExecutor._load_metadata()` or `ToolHandler.extract_metadata()`.

```yaml
# tool_extractor.yaml (excerpt)
parsers:
  .py: python/ast
  .yaml: yaml/yaml
  .yml: yaml/yaml
  .js: javascript/javascript
  .ts: javascript/javascript
  .mjs: javascript/javascript
  .cjs: javascript/javascript
```

## `markdown/xml` — Directive Parser

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

## `markdown/frontmatter` — Knowledge Parser

Parses Markdown files with YAML metadata in ` ```yaml ` code fences (matching how `markdown/xml` uses ` ```xml ` fences for directives). Also handles pure YAML files.

### Input

````markdown
```yaml
name: my-knowledge
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

- Uses `re.search(r"^```yaml\s*$", ...)` to find the fence (mirrors `markdown/xml`'s `_extract_xml_block`)
- YAML inside the fence is parsed with `yaml.safe_load` — full YAML support
- Content below the closing ` ``` ` is the knowledge body
- Pure `.yaml`/`.yml` files: entire content parsed as YAML metadata (signature stripped first)
- `rye_execute(item_type="knowledge")` returns only body content
- `rye_load(item_type="knowledge")` returns full file with metadata

## `python/ast` — Python Tool Parser

Parses Python files using AST to extract dunder metadata and `CONFIG_SCHEMA`.

### Input

```python
# rye:signed:TIMESTAMP:HASH:SIG:FP

"""Tool docstring."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
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

## `yaml/yaml` — YAML Tool Parser

Parses YAML files for runtime configs and tool definitions.

### Input

```yaml
tool_id: rye/core/runtimes/python/script
tool_type: runtime
executor_id: rye/core/primitives/subprocess

env_config:
  interpreter:
    type: local_binary
    binary: python
    candidates: [python3]
    search_paths: [".venv/bin", ".venv/Scripts"]

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

## `javascript/javascript` — JS/TS Tool Parser

Parses JavaScript and TypeScript files using regex to extract `export const` metadata variables. Supports `.js`, `.ts`, `.mjs`, `.cjs` extensions.

### Input

```typescript
// rye:signed:TIMESTAMP:HASH:SIG:FP

export const __version__ = "1.0.0";
export const __tool_type__ = "javascript";
export const __executor_id__ = "rye/core/runtimes/node/node";
export const __category__ = "utility";
export const __tool_description__ = "Example JS/TS tool";

export const CONFIG_SCHEMA = {
  type: "object",
  properties: {
    name: { type: "string", description: "Name to greet" },
  },
  required: ["name"],
};

export const ENV_CONFIG = {
  interpreter: { type: "node", var: "RYE_NODE" },
};

export const CONFIG = {
  timeout: 60,
};
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
| `env_config`       | `ENV_CONFIG`              | dict   |
| `config`           | `CONFIG`                  | dict   |

### Key Behavior

- Uses regex pattern matching — does not execute the file or require a JS runtime
- Matches `export const VAR_NAME = VALUE;` at the module level
- String values extracted from quoted literals
- Object values (`CONFIG_SCHEMA`, `ENV_CONFIG`, `CONFIG`) extracted via balanced-brace matching and parsed as JSON
- Same metadata convention as Python tools (`__dunder__` variables) but with `export const` syntax
- `__executor_id__` typically points to `rye/core/runtimes/node/node`

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
