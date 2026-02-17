**Source:** Original implementation: `.ai/tools/rye/core/extractors/` in kiwi-mcp

# Extractors Category

## Purpose

Extractors are **schema-driven metadata extraction rules** that define WHAT to extract from different content types. They work with the SchemaExtractor engine and Parsers to automatically extract metadata from files.

**Location:** `.ai/tools/rye/core/extractors/`  
**Type:** Schema files (not executable tools)  
**Executor:** `None` (loaded by Bootstrap, not executed directly)  
**Protected:** ✅ Yes (core tool - cannot be shadowed)

## Architecture

```
File (.py, .yaml, .md)
    │
    ├─→ Extractor selected by file extension (EXTENSIONS)
    │
    ├─→ Parser preprocesses content (PARSER → .ai/parsers/{name}.py)
    │   └─→ Returns {"data": {...}, "content": "..."}
    │
    ├─→ EXTRACTION_RULES applied using Primitives
    │   ├─ filename → Extract from file stem
    │   ├─ path → Extract from parsed data dict
    │   ├─ regex → Extract using regex pattern
    │   └─ regex_all → Extract all matches
    │
    └─→ Metadata dict returned
```

## Directory Structure

```
extractors/
├── directive/              # Extract from directive files
│   └── markdown_xml.py     # Directives in markdown with XML
│
├── knowledge/              # Extract from knowledge entries
│   └── markdown_frontmatter.py  # Markdown with YAML frontmatter
│
└── tool/                   # Extract from tool files
    ├── python_extractor.py # Python tools (.py)
    ├── yaml_extractor.py   # YAML configs (.yaml, .yml)
    └── javascript_extractor.py  # JavaScript tools (.js)
```

## Extractor Schema Format

Extractors define these module-level variables:

```python
# .ai/tools/extractors/tool/python_extractor.py

__version__ = "2.0.0"
__tool_type__ = "extractor"
__executor_id__ = None      # NOT executable - loaded by Bootstrap
__category__ = "extractors"

# File extensions this extractor handles
EXTENSIONS = [".py"]

# Parser to preprocess content (loaded from .ai/parsers/)
PARSER = "python_ast"

# How to format validation signatures
SIGNATURE_FORMAT = {
    "prefix": "#",
    "after_shebang": True,
}

# Extraction rules using generic primitives
EXTRACTION_RULES = {
    "name": {"type": "filename"},
    "version": {"type": "ast_var", "name": "__version__"},
    "tool_type": {"type": "ast_var", "name": "__tool_type__"},
    "executor_id": {"type": "ast_var", "name": "__executor_id__"},
    "category": {"type": "ast_var", "name": "__category__"},
    "description": {"type": "ast_docstring"},
    "config_schema": {"type": "ast_var", "name": "CONFIG_SCHEMA"},
    "env_config": {"type": "ast_var", "name": "ENV_CONFIG"},
}

# Optional: Validation schema for extracted data
VALIDATION_SCHEMA = {
    "fields": {
        "name": {"required": True, "type": "string"},
        "version": {"required": True, "type": "semver"},
    }
}
```

## Core Extractors

### 1. Python Extractor (`tool/python_extractor.py`)

**Extensions:** `.py`  
**Parser:** `python_ast`

Extracts metadata from Python module-level variables using AST parsing.

**EXTRACTION_RULES:**
| Field | Primitive | Source |
|-------|-----------|--------|
| `name` | `filename` | File stem |
| `version` | `ast_var` | `__version__` |
| `tool_type` | `ast_var` | `__tool_type__` |
| `executor_id` | `ast_var` | `__executor_id__` |
| `category` | `ast_var` | `__category__` |
| `description` | `ast_docstring` | Module docstring |
| `config_schema` | `ast_var` | `CONFIG_SCHEMA` |
| `env_config` | `ast_var` | `ENV_CONFIG` |

### 2. YAML Extractor (`tool/yaml_extractor.py`)

**Extensions:** `.yaml`, `.yml`  
**Parser:** `yaml`

Extracts metadata from YAML configuration files.

**EXTRACTION_RULES:**
| Field | Primitive | Source |
|-------|-----------|--------|
| `name` | `path` | `tool_id` or `name` |
| `version` | `path` | `version` |
| `tool_type` | `path` | `tool_type` |
| `executor_id` | `path` | `executor_id` |
| `category` | `path` | `category` |
| `description` | `path` | `description` |
| `config_schema` | `path` | `config_schema` |

### 3. Directive Extractor (`directive/markdown_xml.py`)

**Extensions:** `.md`  
**Parser:** `markdown_xml`

Extracts metadata from directives (XML in markdown fenced blocks).

**EXTRACTION_RULES:**
| Field | Primitive | Source |
|-------|-----------|--------|
| `name` | `path` | `name` attribute |
| `version` | `path` | `version` attribute |
| `description` | `path` | `<description>` element |
| `category` | `path` | `<category>` element |
| `inputs` | `path` | `<inputs>` section |
| `process` | `path` | `<process>` section |
| `outputs` | `path` | `<outputs>` section |

### 4. Knowledge Extractor (`knowledge/markdown_frontmatter.py`)

**Extensions:** `.md`  
**Parser:** `markdown_frontmatter`

Extracts metadata from knowledge entries (YAML frontmatter + markdown body).

**EXTRACTION_RULES:**
| Field | Primitive | Source |
|-------|-----------|--------|
| `id` | `path` | `id` frontmatter |
| `title` | `path` | `title` frontmatter |
| `version` | `path` | `version` frontmatter |
| `entry_type` | `path` | `entry_type` frontmatter |
| `category` | `path` | `category` frontmatter |
| `backlinks` | `path` | `_backlinks` (auto-extracted) |
| `body` | `path` | Markdown content after frontmatter |

## Extraction Primitives

The SchemaExtractor engine provides these generic primitives:

| Primitive | Description | Required Params |
|-----------|-------------|-----------------|
| `filename` | Extract from file stem | None |
| `path` | Extract from parsed data dict | `key` |
| `regex` | Extract using regex pattern | `pattern` |
| `regex_all` | Extract all regex matches | `pattern` |
| `category_path` | Extract category from file path | `base_folder` |
| `ast_var` | Extract Python module variable | `name` (via parser) |
| `ast_docstring` | Extract Python docstring | None (via parser) |

## How Extraction Works

### 1. Bootstrap Loads Extractor

```python
# Engine loads extractor by executing file
extractor = Bootstrap.load_extractor(Path("python_extractor.py"))
# Returns: {
#   "extensions": [".py"],
#   "parser": "python_ast",
#   "rules": {...EXTRACTION_RULES...},
#   "validation_schema": {...}
# }
```

### 2. Parser Preprocesses Content

```python
# Engine loads parser from .ai/parsers/
parser = get_parser("python_ast")
parsed = parser(file_content)
# Returns: {"data": {"__version__": "1.0.0", ...}, "content": "..."}
```

### 3. Rules Extract Metadata

```python
# SchemaExtractor applies rules
for field, rule in EXTRACTION_RULES.items():
    primitive = PRIMITIVES[rule["type"]]
    metadata[field] = primitive(rule, parsed, file_path)
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Type** | Schema files (not executable) |
| **Executor** | `None` (loaded by Bootstrap) |
| **Purpose** | Define extraction rules |
| **Parser** | Points to `.ai/parsers/{name}.py` |
| **Primitives** | Generic extraction functions |
| **Self-describing** | Extractors define their own schemas |

## Related Documentation

- [core/parsers](parsers.md) - Parsers that preprocess content
- [../bundle/structure](../bundle/structure.md) - Bundle organization
- [overview](overview.md) - All categories
