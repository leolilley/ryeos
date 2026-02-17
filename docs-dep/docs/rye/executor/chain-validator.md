# ChainValidator

## Purpose

ChainValidator validates tool execution chains before running them. It ensures that parent→child relationships are compatible (outputs match inputs) and prevents runtime failures from incompatible tool combinations.

## Architecture Role

ChainValidator sits in RYE's executor layer, validating chains **before** execution begins:

```
┌─────────────────────────────────────────────┐
│  execute(item_type="tool", item_id="...")   │
└──────────────────┬──────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────┐
│  Executor builds chain                      │
│  tool → runtime → primitive                 │
└──────────────────┬──────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────┐
│  ChainValidator.validate_chain(chain)       │  ← Pre-execution validation
│  - Check I/O compatibility                  │
│  - Verify version constraints               │
│  - Uses Lilux hash functions for integrity  │
└──────────────────┬──────────────────────────┘
                   │
           valid?  │
      ┌────────────┴────────────┐
      ▼                         ▼
   Execute                   Fail fast
   (via Lilux)               (with errors)
```

## Key Classes

### ChainValidationResult

```python
@dataclass
class ChainValidationResult:
    valid: bool                    # Whether chain is valid
    issues: List[str]             # Fatal validation errors
    warnings: List[str]           # Non-fatal warnings
    validated_pairs: int          # Number of (parent, child) pairs checked
```

### ChainValidator

```python
class ChainValidator:
    def validate_chain(
        self, 
        chain: List[Dict[str, Any]]
    ) -> ChainValidationResult:
        """Validate each (child, parent) pair in the chain."""
```

## Integration with On-Demand Loading

ChainValidator works with RYE's on-demand loading model. When executor builds a chain:

1. **Tool loaded on-demand** from `.ai/tools/`
2. **Metadata parsed** (`__executor_id__`, `CONFIG_SCHEMA`)
3. **Chain constructed** (tool → runtime → primitive)
4. **Space tracking applied** - Each chain element includes which space it's from
5. **ChainValidator validates** before execution
6. **Execute or fail fast**

```python
# In executor (pseudocode)
chain = build_chain(item_id)  # On-demand loading
result = chain_validator.validate_chain(chain)

if not result.valid:
    raise ChainValidationError(result.issues)

# Safe to execute
return execute_chain(chain)
```

**See Also:** [[./tool-resolution-and-validation.md]] for complete details on tool spaces, precedence, and cross-space validation.

---

## Tool Spaces and Chain Validation

When tools exist across multiple spaces (project/user/system), chain validation ensures dependencies are valid:

### Space Compatibility Rules

| Child Space | Parent Space | Valid? | Reason |
|-------------|---------------|---------|---------|
| project | user | ✅ Yes | Project has higher precedence than user |
| project | system | ✅ Yes | Project has higher precedence than system |
| user | system | ✅ Yes | User has higher precedence than system |
| user | project | ❌ No | User cannot depend on project-specific tools |
| system | project | ❌ No | System immutable, cannot depend on mutable project tools |
| system | user | ❌ No | System immutable, cannot depend on mutable user tools |
| project | project | ✅ Yes | Same space |
| user | user | ✅ Yes | Same space |
| system | system | ✅ Yes | Same space |

**Key Insight:**
- A tool can always depend on tools from **equal or higher precedence spaces**
- A tool CANNOT depend on tools from **lower precedence spaces**
- This prevents circular dependencies and ensures stability

### Space Compatibility Validation

```python
def _validate_space_compatibility(
    self,
    child: Dict,
    parent: Dict,
    result: ChainValidationResult
):
    """
    Validate that tools from different spaces are compatible.
    """
    child_space = child.get("space", "")
    parent_space = parent.get("space", "")

    precedence = {"project": 3, "user": 2, "system": 1}

    # Lower precedence depending on higher precedence: Invalid
    if precedence.get(child_space, 0) < precedence.get(parent_space, 0):
        result.issues.append(
            f"Tool '{child['tool_id']}' from {child_space} space cannot "
            f"depend on '{parent['tool_id']}' from {parent_space} space. "
            f"Use a higher-precedence space version or pin the dependency."
        )
        result.valid = False
```

## Lilux Integration

ChainValidator uses Lilux's pure hash functions for integrity verification:

```python
from lilux.primitives import compute_hash

# Validate tool hasn't been tampered with
expected_hash = tool_metadata.get("content_hash")
actual_hash = compute_hash(tool_content)

if expected_hash and expected_hash != actual_hash:
    result.issues.append(f"Integrity check failed for {tool_id}")
```

**See Also:** [lilux/primitives/integrity](../lilux/primitives/integrity.md) for hash function details

## The Problem: Incompatible Tools

Without validation:

```
Tool A outputs: JSON object
Tool B expects: JSON array

A → B → Runtime crash!
```

With ChainValidator:

```
Tool A outputs: JSON object
Tool B expects: JSON array

ChainValidator detects mismatch → Error BEFORE execution
```

## Execution Chain Structure

RYE tools form a chain from leaf (user tool) to root (primitive):

```
[user_tool, runtime, primitive]
     ↓         ↓         ↓
  "git.py"  "python"  "subprocess"
  
Chain[0] ← delegated by
Chain[1] ← delegated by
Chain[2] (primitive, root)
```

ChainValidator checks each adjacent pair:
1. Can `chain[1]` (runtime) call `chain[0]` (user tool)?
2. Can `chain[2]` (primitive) call `chain[1]` (runtime)?

## Usage

```python
from rye.executor import ChainValidator

validator = ChainValidator()

# Validate a chain before execution
result = validator.validate_chain(tool_chain)

if not result.valid:
    for issue in result.issues:
        logger.error(f"Chain error: {issue}")
    raise ChainValidationError(result)

# Safe to execute
execute_chain(tool_chain)
```

## Example: Valid Chain

```python
chain = [
    {
        "item_id": "csv_reader",
        "item_type": "tool",
        "outputs": ["json_array"]
    },
    {
        "item_id": "python_runtime",
        "item_type": "runtime",
        "inputs": ["json_array"],
        "outputs": ["json_object"]
    },
    {
        "item_id": "subprocess",
        "item_type": "primitive",
        "inputs": ["json_object"]
    }
]

result = validator.validate_chain(chain)

assert result.valid == True
assert len(result.issues) == 0
assert result.validated_pairs == 2
```

## Example: Invalid Chain

```python
chain = [
    {
        "item_id": "csv_reader",
        "item_type": "tool",
        "outputs": ["json_array"]  # Outputs array
    },
    {
        "item_id": "json_processor",
        "item_type": "tool",
        "inputs": ["json_object"],  # Expects object!
        "outputs": ["json_object"]
    }
]

result = validator.validate_chain(chain)

assert result.valid == False
assert len(result.issues) > 0
assert "json_object" in result.issues[0]
```

## Validation Rules

### Input/Output Matching

Child's outputs must match parent's inputs:

```python
# Valid - exact match
child.outputs = ["json"]
parent.inputs = ["json"]

# Valid - child provides more than needed
child.outputs = ["json", "xml"]
parent.inputs = ["json"]

# Invalid - parent doesn't get what it needs
child.outputs = ["xml"]
parent.inputs = ["json"]
```

### Version Constraints

Tools can specify version requirements:

```python
parent_def = {
    "item_id": "processor",
    "item_type": "tool",
    "child_constraints": {
        "reader": {
            "min_version": "1.0.0",
            "max_version": "2.0.0"
        }
    }
}

# If reader is v0.9.0 or v2.1.0, validation fails
```

## Error Types

### Missing Output

```python
chain = [
    {"item_id": "reader", "item_type": "tool", "outputs": ["data"]},
    {"item_id": "processor", "item_type": "tool", "inputs": ["data", "config"]}
]

# processor expects "config" but reader doesn't provide it
result = validator.validate_chain(chain)
assert result.valid == False
assert "config" in result.issues[0]
```

### Type Mismatch

```python
chain = [
    {"item_id": "reader", "item_type": "tool", "outputs": ["json_array"]},
    {"item_id": "processor", "item_type": "tool", "inputs": ["json_object"]}
]

result = validator.validate_chain(chain)
assert result.valid == False
assert "type mismatch" in result.issues[0].lower()
```

## Best Practices

### 1. Define Schemas in Tools

```python
# .ai/tools/my_tool.py
__tool_type__ = "python"
__executor_id__ = "python_runtime"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "input": {"type": "array"},
        "output": {"type": "object"}
    }
}
```

### 2. Validate Early

```python
result = validator.validate_chain(chain)
if not result.valid:
    # Fail fast before any execution
    raise ChainValidationError(result)
```

### 3. Log Warnings

```python
result = validator.validate_chain(chain)
for warning in result.warnings:
    logger.warning(f"Chain warning: {warning}")
```

## Lazy Loading

ChainValidator uses lazy loading for SchemaValidator:

```python
# First use - loads SchemaValidator
validator = ChainValidator()
result = validator.validate_chain(chain)  # Loads now

# Second use - already loaded
result = validator.validate_chain(other_chain)  # Fast
```

## Limitations (By Design)

1. **No runtime enforcement** - Validates structure, doesn't monitor execution
2. **Schema optional** - Works with or without detailed schemas
3. **No ordering** - Doesn't validate tool order; executor handles ordering

## Related Documentation

- [overview](overview.md) - Executor architecture
- [routing](routing.md) - How chains are built
- [lilux/primitives/integrity](../lilux/primitives/integrity.md) - Hash functions for verification
- [../principles.md](../principles.md) - On-demand loading model
