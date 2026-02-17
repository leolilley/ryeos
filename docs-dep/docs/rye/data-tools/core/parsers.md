**Source:** Original implementation: `.ai/tools/rye/core/parsers/` in kiwi-mcp

# Parsers Category

## Purpose

Parsers are **content preprocessors** that transform raw file content into structured data for extraction. They are simple Python modules with a `parse()` function.

**Location:** `.ai/tools/rye/core/parsers/`  
**Type:** Python modules with `parse()` function  
**Loading:** Dynamic loading by SchemaExtractor engine  
**Protected:** ✅ Yes (core tool - cannot be shadowed)

## Architecture

```
File Content (raw string)
    │
    ├─→ Extractor specifies PARSER = "python_ast"
    │
    ├─→ Engine loads .ai/parsers/python_ast.py
    │
    ├─→ parser.parse(content) called
    │   └─→ Returns {"data": {...}, "content": "..."}
    │
    └─→ Extraction rules applied to parsed data
```

## Parser Interface

All parsers must implement this interface:

```python
# .ai/parsers/{name}.py

def parse(content: str) -> dict:
    """
    Parse content and return structured data.
    
    Args:
        content: Raw file content as string
        
    Returns:
        dict with at least:
        - "data": dict of extracted values (for "path" primitive)
        - "content": original or processed content (for "regex" primitive)
        
        May also include:
        - "body": markdown body (for knowledge entries)
        - Any other parser-specific fields
    """
    pass
```

## Core Parsers

### 1. Python AST Parser (`python_ast.py`)

**Purpose:** Parse Python files using Abstract Syntax Tree

**Input:** Python source code  
**Output:** Module-level variables and docstring

```python
def parse(content):
    """Parse Python AST and extract module-level variables."""
    tree = ast.parse(content)
    data = {}

    # Extract module-level variable assignments
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1:
            target = node.targets[0]
            if isinstance(target, ast.Name):
                try:
                    data[target.id] = ast.literal_eval(node.value)
                except (ValueError, TypeError):
                    pass

    # Extract module docstring
    if tree.body and isinstance(tree.body[0], ast.Expr):
        if isinstance(tree.body[0].value, ast.Constant):
            if isinstance(tree.body[0].value.value, str):
                data["_docstring"] = tree.body[0].value.value.strip()

    return {"data": data, "content": content}
```

**Example Output:**
```python
{
    "data": {
        "__version__": "1.0.0",
        "__tool_type__": "python",
        "__executor_id__": "python_runtime",
        "CONFIG_SCHEMA": {"type": "object", ...},
        "_docstring": "Tool description from docstring."
    },
    "content": "...original source..."
}
```

### 2. YAML Parser (`yaml.py`)

**Purpose:** Parse YAML configuration files

**Input:** YAML content  
**Output:** Parsed YAML as dict

```python
def parse(content):
    """Parse YAML content."""
    try:
        return {"data": yaml.safe_load(content) or {}, "content": content}
    except Exception:
        return {"data": {}, "content": content}
```

**Example Output:**
```python
{
    "data": {
        "name": "mcp_stdio",
        "version": "1.0.0",
        "tool_type": "runtime",
        "executor_id": "subprocess",
        "config": {...}
    },
    "content": "...original yaml..."
}
```

### 3. Markdown Frontmatter Parser (`markdown_frontmatter.py`)

**Purpose:** Parse markdown with YAML frontmatter and extract backlinks

**Input:** Markdown with `---` frontmatter  
**Output:** Frontmatter dict + body + backlinks

```python
def parse(content):
    """Parse markdown with YAML frontmatter."""
    # Skip signature comment if present
    # Extract frontmatter between --- markers
    # Extract backlinks from body using [[link]] pattern
    
    return {
        "data": {
            **frontmatter,
            "_backlinks": backlinks,
            "body": body
        },
        "body": body,
        "content": content
    }
```

**Example Output:**
```python
{
    "data": {
        "id": "K-12345",
        "title": "My Knowledge Entry",
        "version": "1.0.0",
        "entry_type": "learning",
        "_backlinks": ["other-entry", "related-topic"],
        "body": "# Content\n\nMarkdown body here..."
    },
    "body": "# Content\n\nMarkdown body here...",
    "content": "...original file..."
}
```

### 4. Markdown XML Parser (`markdown_xml.py`)

**Purpose:** Parse directives (XML embedded in markdown fenced blocks)

**Input:** Markdown with ```xml fenced block containing directive  
**Output:** Parsed directive structure

**Features:**
- Extracts XML from markdown fenced blocks
- Masks opaque sections (template, example) before parsing
- Handles nested fences and special characters
- Parses directive metadata, inputs, process, outputs

```python
def parse(file_content: str) -> dict:
    """Parse directive XML from markdown."""
    # Extract XML from ```xml fence
    # Mask opaque sections (template, example)
    # Parse with ElementTree
    # Reattach opaque content
    
    return {
        "data": {
            "name": "directive_name",
            "version": "1.0.0",
            "description": "...",
            "inputs": [...],
            "process": {...},
            "outputs": {...},
            "templates": {...}
        },
        "body": body_before_fence,
        "raw": file_content
    }
```

## Parser Loading

Parsers are loaded dynamically with mtime-based caching:

```python
# Engine searches for parser in order:
# 1. {project}/.ai/parsers/{name}.py
# 2. {cwd}/.ai/parsers/{name}.py
# 3. {user_space}/parsers/{name}.py

parser = get_parser("python_ast", project_path)
result = parser(file_content)
```

**Cache Invalidation:** Parser is reloaded if file mtime changes.

## Creating New Parsers

1. Create `.ai/parsers/{name}.py`
2. Implement `parse(content: str) -> dict`
3. Return `{"data": {...}, "content": "..."}`
4. Reference in extractor: `PARSER = "{name}"`

**Example: JSON Parser**
```python
# .ai/parsers/json.py
import json

def parse(content):
    """Parse JSON content."""
    try:
        return {"data": json.loads(content), "content": content}
    except Exception:
        return {"data": {}, "content": content}
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
 | **Location** | `.ai/tools/rye/core/parsers/` |
| **Interface** | `parse(content: str) -> dict` |
| **Loading** | Dynamic with mtime caching |
| **Purpose** | Preprocess content for extraction |
| **Output** | `{"data": {...}, "content": "..."}` |

## Builtin Parsers

| Parser | Purpose | Used By |
|--------|---------|---------|
| `text` | No preprocessing (builtin) | Fallback |
| `python_ast` | Parse Python AST | Python tools |
| `yaml` | Parse YAML | YAML configs |
| `markdown_frontmatter` | Parse frontmatter + backlinks | Knowledge entries |
| `markdown_xml` | Parse directive XML from markdown | Directives |

## Related Documentation

- [core/extractors](extractors.md) - Extractors that use parsers
- [../bundle/structure](../bundle/structure.md) - Bundle organization
- [overview](overview.md) - All categories
