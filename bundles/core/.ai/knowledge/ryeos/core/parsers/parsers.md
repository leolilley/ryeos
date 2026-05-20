---
category: ryeos/core
tags: [reference, parsers, formats, parsing]
version: "1.0.0"
description: >
  How file parsing works — parsers, handlers, and the multi-format
  metadata extraction pipeline.
---

# Parsers and Formats

Rye OS supports multiple file formats through a two-layer parsing
system: **parsers** (declarations) and **handlers** (implementations).

## Architecture

```
File → Kind → Parser → Handler → Metadata dict
```

1. The **kind** schema maps file extensions to parsers
2. The **parser** declaration specifies which handler to invoke
3. The **handler** extracts metadata from the file content
4. The result is a normalized metadata dictionary

## Built-in Parsers

### YAML Parser (`parser:ryeos/core/yaml/yaml`)
- **Handler:** `handler:ryeos/core/yaml-document`
- **Extensions:** `.yaml`, `.yml`
- **Behavior:** Parses the entire file as a YAML mapping. The most
  generic parser — used by config, handler, kind, node, parser,
  protocol, runtime, service, and tool kinds.

### Python AST Parser (`parser:ryeos/core/python/ast`)
- **Handler:** `handler:ryeos/core/regex-kv`
- **Extensions:** `.py`
- **Behavior:** Extracts module-level dunder constants
  (`__version__ = "1.0.0"`) using regex. Keys are stripped of `__`
  framing so `__version__` becomes `version`. Also extracts docstrings.

### JavaScript Parser (`parser:ryeos/core/javascript/javascript`)
- **Handler:** `handler:ryeos/core/regex-kv`
- **Extensions:** `.js`, `.ts`, `.mjs`, `.cjs`
- **Behavior:** Extracts `const __X__ = "value"` assignments using regex.
  Also extracts `CONFIG_SCHEMA`, `ENV_CONFIG`, and `CONFIG` objects
  with balanced-brace matching and JS-to-JSON conversion.

### Markdown Directive Parser (`parser:ryeos/core/markdown/directive`)
- **Handler:** `handler:ryeos/core/yaml-header-document`
- **Extensions:** `.md`
- **Behavior:** Extracts YAML header (either `---` frontmatter or
  ` ```yaml ` fenced block) plus the body text. Used for directive
  files where the header defines metadata and the body is the LLM
  prompt. Header is required.

### Markdown Frontmatter Parser (`parser:ryeos/core/markdown/frontmatter`)
- **Handler:** `handler:ryeos/core/yaml-header-document`
- **Extensions:** `.md`
- **Behavior:** Extracts optional YAML from ` ```yaml ` fenced blocks
  in markdown. Used for knowledge entries. Header is optional — plain
  markdown without metadata is valid.

## Handlers

Handlers are the executable backends that parsers delegate to:

| Handler                        | Serves   | Description                              |
|--------------------------------|----------|------------------------------------------|
| `yaml-document`                | parser   | Parse entire file as YAML mapping        |
| `yaml-header-document`         | parser   | Parse YAML header + body (markdown)      |
| `regex-kv`                     | parser   | Extract key-value pairs via regex         |
| `extends-chain`                | composer | Resolve inheritance chains               |
| `graph-permissions`            | composer | Lift graph permissions into policy facts  |
| `identity`                     | composer | No-op pass-through                       |

## Format Normalization

Regardless of file format, all tools produce the same metadata shape.
A Python tool's `__version__ = "1.0.0"` and a YAML tool's `version: "1.0.0"`
both result in `{"version": "1.0.0"}` in the parsed metadata.

This means you can convert a tool from Python to YAML (or vice versa)
without changing how the engine sees it.
