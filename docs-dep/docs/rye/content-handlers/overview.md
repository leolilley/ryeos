**Source:** Original implementation: `kiwi_mcp/handlers/` in kiwi-mcp

# Content Handlers Overview

## Purpose

Content handlers provide **intelligent parsing and understanding** of different content formats used in RYE.

## Content Handler Architecture

```
RYE Content
    │
    ├─→ XML Content (Directives)
    │   └─→ XML Handler
    │       ├─ Parse XML structure
    │       ├─ Extract metadata
    │       └─ Validate schema
    │
    ├─→ Markdown Content (Knowledge)
    │   └─→ Frontmatter Handler
    │       ├─ Parse YAML frontmatter
    │       ├─ Parse Markdown body
    │       └─ Extract metadata
    │
    └─→ Multi-Format Content (Tools)
        └─→ Metadata Extractors
            ├─ Parse Python AST (.py)
            ├─ Parse YAML config (.yaml, .yml)
            ├─ Parse JavaScript (.js)
            ├─ Parse Bash scripts (.sh)
            ├─ Parse TOML (.toml)
            ├─ Parse JSON (.json)
            ├─ Parse XML (.xml)
            └─ Extract metadata
```

## Handler Types

### 1. XML Handler

**Purpose:** Parse XML directives

**Handles:**

- Directive definitions
- XML structure validation
- Element extraction

**Key Features:**

- Schema validation
- Category support
- Element traversal

### 2. Frontmatter Handler

**Purpose:** Parse YAML frontmatter + Markdown content

**Handles:**

- Knowledge entries
- YAML frontmatter
- Markdown body
- Metadata extraction

**Key Features:**

- Separator detection (---)
- YAML parsing
- Markdown parsing
- Frontmatter validation

### 3. Metadata Extractors

**Purpose:** Extract metadata from tools in any format

**Handles:**

- Python files (.py)
- YAML config files (.yaml, .yml)
- JavaScript files (.js)
- Bash scripts (.sh)
- TOML files (.toml)
- JSON files (.json)
- XML files (.xml)
- Metadata attributes
- Schema extraction

**Key Features:**

- AST parsing (Python)
- YAML structure reading (YAML files)
- Module variable extraction (JavaScript)
- Shebang and parameter parsing (Bash)
- Metadata validation
- Schema extraction
- Format-agnostic design (extensible to any format)

## Usage Pattern

Content handlers are used internally by extractors:

```
Content File (XML, Markdown, Python, YAML, JS, Bash, TOML, JSON, etc.)
    │
    ├─→ Content Handler parses content
    │
    ├─→ Returns parsed structure
    │
    └─→ Extractor uses structure
        └─→ Returns metadata
```

## Parsing Pipeline

### Directive Parsing

```
.xml file
    │
    └─→ XML Handler
        ├─ Load XML
        ├─ Validate schema
        └─→ Extract:
            ├─ name
            ├─ version
            ├─ description
            ├─ inputs
            ├─ process
            └─ outputs
```

### Knowledge Parsing

```
.md file
    │
    └─→ Frontmatter Handler
        ├─ Split frontmatter & body
        ├─ Parse YAML frontmatter
        ├─ Parse Markdown body
        └─→ Extract:
            ├─ zettel_id
            ├─ title
            ├─ entry_type
            ├─ tags
            ├─ references
            └─ content
```

### Tool Metadata Extraction

Tools support multiple formats with dedicated extractors:

```
.py file
    │
    └─→ Python Extractor
        └─ Extract:
            ├─ __version__
            ├─ __tool_type__
            ├─ __executor_id__
            ├─ __category__
            ├─ CONFIG_SCHEMA
            └─ ENV_CONFIG

.yaml, .yml file
    │
    └─→ YAML Extractor
        └─ Extract:
            ├─ name
            ├─ version
            ├─ tool_type
            ├─ executor_id
            ├─ category
            ├─ config_schema
            └─ env_config

.js file
    │
    └─→ JavaScript Extractor
        └─ Extract:
            ├─ __version__
            ├─ __tool_type__
            ├─ __executor_id__
            ├─ __category__
            ├─ CONFIG_SCHEMA
            └─ ENV_CONFIG

.sh, .toml, .json, .xml files
    │
    └─→ Format-Specific Extractor (extensible)
        └─ Extract metadata per format
```

## Handler Integration

Handlers work with Parsers and Extractors:

```
Content Files
    │
    ├─→ Handlers (parse format)
    │   ├─ XML Handler
    │   ├─ Frontmatter Handler
    │   └─ Metadata Handler
    │
    ├─→ Parsers (format-specific)
    │   ├─ Markdown XML Parser
    │   ├─ Frontmatter Parser
    │   ├─ Python AST Parser
    │   └─ YAML Parser
    │
    └─→ Extractors (content-specific)
        ├─ Directive Extractor
        ├─ Knowledge Extractor
        └─ Tool Extractor
```

## Key Characteristics

| Aspect          | Detail                                                              |
| --------------- | ------------------------------------------------------------------- |
| **Purpose**     | Parse content formats                                               |
| **Formats**     | XML, Markdown, Python, YAML, JavaScript, Bash, TOML, JSON, and more |
| **Integration** | Used by Parsers and Extractors                                      |
| **Validation**  | Schema validation included                                          |
| **Metadata**    | Extract structured metadata                                         |

## Content Handler Standards

### Input

- File path or content string
- Optional validation schema

### Processing

1. Parse format
2. Validate structure
3. Extract metadata
4. Return structured data

### Output

```python
{
    "format": "xml" | "markdown" | "python" | "yaml" | "javascript" | "bash" | "toml" | "json" | "xml",
    "valid": true | false,
    "metadata": {...},
    "content": {...},
    "errors": [],
    "warnings": []
}
```

## Error Handling

Handlers provide detailed error information:

```python
{
    "valid": False,
    "errors": [
        {
            "type": "ParseError",
            "message": "Invalid XML structure",
            "line": 5,
            "column": 12
        }
    ],
    "warnings": [
        {
            "type": "DeprecatedField",
            "message": "Field 'old_name' is deprecated"
        }
    ]
}
```

## RAG Integration Hooks

Content handlers can optionally integrate with RAG tools for automatic indexing.

### Knowledge Handler Hook

When users enable auto-indexing, the knowledge handler calls RAG tools:

```python
# In RYE's knowledge handler
async def create_knowledge(entry):
    # 1. Always save knowledge (RAG or not)
    await save_knowledge(entry)

    # 2. Check if user has RAG configured and enabled
    rag_config = load_config(".ai/config/rag.yaml")
    if not rag_config or not rag_config.get("auto_index", {}).get("enabled"):
        return  # No auto-indexing

    # 3. Check if knowledge auto-indexing is enabled
    if not rag_config.get("auto_index", {}).get("collections", {}).get("knowledge"):
        return  # Knowledge auto-indexing disabled

    # 4. Check if rag_index tool exists
    if not tool_exists("rag_index"):
        logger.debug("rag_index tool not found, skipping auto-indexing")
        return

    # 5. Call RAG tool to index
    try:
        await execute("rag_index", {
            "documents": [{
                "id": entry["id"],
                "content": entry["content"],
                "metadata": {
                    "title": entry.get("title"),
                    "category": entry.get("category"),
                    "entry_type": entry.get("entry_type"),
                    "created_at": entry.get("created_at")
                }
            }],
            "collection": "knowledge"
        })
        logger.info(f"Auto-indexed knowledge entry: {entry['id']}")
    except Exception as e:
        # RAG failed, but knowledge is still saved
        logger.warning(f"RAG auto-indexing failed: {e}")
```

**Key Points:**

- ✅ Knowledge is **always saved** regardless of RAG
- ✅ RAG indexing is **optional** and **non-blocking**
- ✅ Failures in RAG don't affect knowledge creation
- ✅ User controls via configuration
- ✅ RAG tools are called as data-driven tools (not hardcoded)

### Hook Configuration

```yaml
# .ai/config/rag.yaml
auto_index:
  enabled: true
  collections:
    knowledge: true # Enable knowledge auto-indexing
    tools: false # Disable tool auto-indexing
```

---

## Related Documentation

- **RAG Tools:** `[[../categories/rag]]` - RAG tool documentation
- **RAG Configuration:** `[[../config/overview]]` - Configuration system
- **Parsers:** `[[../categories/parsers]]` - Data format parsers
- **Extractors:** `[[../categories/extractors]]` - Data extraction utilities
- **Executor:** `[[../executor/overview]]` - Executor architecture
- **Bundle:** `[[../bundle/structure]]` - Bundle organization
