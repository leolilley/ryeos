# Schemas Overview

## Purpose

Lilux provides JSON Schema validation utilities. The orchestrator (Rye) provides schemas to Lilux for validation - Lilux never resolves tool IDs or discovers schemas.

> **Important:** Lilux never resolves tool IDs or discovers schemas. The orchestrator provides schemas to Lilux for validation.

## Public API

Lilux exports these schema utilities:

```python
from lilux.schemas import (
    SchemaValidator,           # Class for validation
    SchemaExtractor,           # Class for metadata extraction
    extract_tool_metadata,     # Convenience function
    validate_tool_metadata,    # Convenience function
    extract_and_validate,      # Combined extraction + validation
)
```

### SchemaValidator

Pure validation against a provided schema:

```python
from lilux.schemas import SchemaValidator

validator = SchemaValidator()

# Orchestrator provides the schema (from tool registry, config, etc.)
schema = {
    "type": "object",
    "properties": {
        "file": {
            "type": "string",
            "description": "Path to CSV file"
        },
        "delimiter": {
            "type": "string",
            "default": ",",
            "description": "Field delimiter"
        }
    },
    "required": ["file"]
}

# Validate data against provided schema
result = validator.validate(data={"file": "data.csv", "delimiter": ";"})

# Result structure:
# result["valid"]    → bool
# result["issues"]   → List[str] (validation errors)
# result["warnings"] → List[str] (non-fatal warnings)
```

### Convenience Functions

```python
from lilux.schemas import extract_tool_metadata, validate_tool_metadata

# Extract metadata from a tool file
metadata = extract_tool_metadata(Path("tool.py"), item_type="tool")

# Validate extracted metadata
result = validate_tool_metadata(metadata)
if not result["valid"]:
    print(f"Errors: {result['issues']}")
```

## Schema Structure

### Complete Tool Schema

```json
{
  "tool_id": "process_data",
  "version": "1.0.0",
  "name": "Data Processor",
  "description": "Process and transform data",
  "executor": "subprocess",
  "config": {
    "command": "python",
    "args": ["processor.py"]
  },
  "inputs": {
    "type": "object",
    "properties": {
      "input_file": {
        "type": "string",
        "description": "Input file path"
      },
      "format": {
        "type": "string",
        "enum": ["csv", "json", "parquet"],
        "default": "csv"
      },
      "validate": {
        "type": "boolean",
        "default": true
      }
    },
    "required": ["input_file"]
  },
  "outputs": {
    "type": "object",
    "properties": {
      "data": {
        "type": "array",
        "items": { "type": "object" }
      },
      "errors": {
        "type": "array",
        "items": { "type": "string" }
      }
    }
  }
}
```

## Schema Validation

### Validate Parameters

```python
from lilux.schemas import SchemaValidator

validator = SchemaValidator()

# Schema from tool
schema = {
    "type": "object",
    "properties": {
        "count": {"type": "integer"},
        "name": {"type": "string"}
    },
    "required": ["name"]
}

# Validate parameters
result = validator.validate(
    instance={"name": "Alice", "count": 10},
    schema=schema
)

assert result.valid  # True
```

### Validation Errors

```python
# Missing required field
result = validator.validate(
    instance={"count": 10},  # Missing "name"
    schema=schema
)

assert not result.valid
assert len(result.errors) > 0
# Error: 'name' is a required property
```

## Architecture Role

Lilux provides **validation utilities only**. The orchestrator (Rye) handles:

1. **Tool discovery** - Finding available tools
2. **Schema loading** - Retrieving schemas by tool ID/version
3. **Execution decisions** - When and what to validate

Lilux provides:

1. **Parameter validation** - Check inputs against provided schema
2. **Type checking** - Ensure correct types
3. **Error reporting** - Clear validation error messages

## Usage Patterns

### Tool Author Workflow

Tool authors can directly use `SchemaValidator` to validate parameters before execution:

```python
from lilux.schemas import SchemaValidator

# Tool defines its schema
tool_schema = {
    "type": "object",
    "properties": {
        "file": {"type": "string"},
        "format": {
            "type": "string",
            "enum": ["csv", "json"],
            "default": "csv"
        }
    },
    "required": ["file"]
}

# Tool author validates their own parameters
validator = SchemaValidator()
result = validator.validate(user_params, tool_schema)

if not result.valid:
    raise ValueError(f"Invalid parameters: {result.errors}")

# Then execute primitive (if validation passed)
result = await primitive.execute(config, user_params)
```

### Orchestrator Workflow

Orchestrators load schemas and validate before tool execution:

```python
# Orchestrator loads schema (Lilux doesn't do this)
schema = orchestrator.load_tool_schema(tool_id, version)

# Lilux validates against provided schema
validator = SchemaValidator()
result = validator.validate(instance=user_params, schema=provided_schema)

if not result.valid:
    raise ValueError(f"Invalid parameters: {result.errors}")

# Orchestrator executes with validated parameters
result = await orchestrator.execute_tool(tool_id, user_params)
```

## Best Practices

### 1. Define Clear Schemas

```json
{
  "properties": {
    "file_path": {
      "type": "string",
      "description": "Path to input file",
      "pattern": "^[/.].*\\.(csv|json|txt)$"
    },
    "encoding": {
      "type": "string",
      "default": "utf-8",
      "enum": ["utf-8", "latin-1", "ascii"]
    }
  },
  "required": ["file_path"]
}
```

### 2. Provide Default Values

```json
{
  "properties": {
    "timeout": {
      "type": "integer",
      "default": 300,
      "minimum": 1,
      "maximum": 3600
    },
    "retries": {
      "type": "integer",
      "default": 3
    }
  }
}
```

### 3. Document with Descriptions

```json
{
  "properties": {
    "api_key": {
      "type": "string",
      "description": "API key for authentication (from $API_KEY env var)"
    },
    "endpoint": {
      "type": "string",
      "description": "API endpoint URL (default: https://api.example.com)"
    }
  }
}
```

## JSON Schema Features Used

### Data Types

```json
{
  "properties": {
    "name": { "type": "string" },
    "age": { "type": "integer" },
    "score": { "type": "number" },
    "active": { "type": "boolean" },
    "tags": { "type": "array", "items": { "type": "string" } },
    "config": { "type": "object", "properties": {...} }
  }
}
```

### Constraints

```json
{
  "properties": {
    "age": {
      "type": "integer",
      "minimum": 0,
      "maximum": 150
    },
    "email": {
      "type": "string",
      "pattern": "^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}$"
    },
    "format": {
      "type": "string",
      "enum": ["csv", "json", "xml"]
    }
  }
}
```

### Required Fields

```json
{
  "properties": {
    "file": { "type": "string" },
    "delimiter": { "type": "string", "default": "," }
  },
  "required": ["file"] // file is required, delimiter is optional
}
```

## Testing

```python
import pytest
from lilux.schemas import SchemaValidator

def test_validate_tool_parameters():
    validator = SchemaValidator()

    schema = {
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "count": {"type": "integer", "minimum": 1}
        },
        "required": ["name"]
    }

    # Valid
    result = validator.validate(
        {"name": "test", "count": 5},
        schema
    )
    assert result.valid

    # Invalid - missing required
    result = validator.validate(
        {"count": 5},
        schema
    )
    assert not result.valid

    # Invalid - wrong type
    result = validator.validate(
        {"name": "test", "count": "five"},
        schema
    )
    assert not result.valid
```

## Limitations and Design

### By Design (Not a Bug)

1. **Lilux provides validation only**
   - Pure JSON Schema validation
   - No tool ID resolution or schema discovery
   - Schema must be provided by orchestrator

2. **Orchestrator handles discovery**
   - Tool registry lookups
   - Schema loading by ID/version
   - Execution decisions

3. **No automatic generation**
   - Schemas must be written manually
   - Or generated by external tooling

## Next Steps

- See tool schema file: `[[lilux/schemas/tool-schema]]`
- See runtime services: `[[lilux/runtime-services/overview]]`
