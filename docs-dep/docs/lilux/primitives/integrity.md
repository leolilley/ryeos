# Integrity Primitive

## Purpose

Compute deterministic SHA256 hashes for tools, directives, and knowledge entries. Integrity hashes enable content-addressed storage and change detection.

## Key Functions

### compute_tool_integrity()

Compute integrity hash for a tool version:

```python
def compute_tool_integrity(
    tool_id: str,
    version: str,
    manifest: Dict[str, Any],
    files: Optional[List[Dict[str, Any]]] = None
) -> str:
    """
    Compute deterministic integrity hash for a tool version.

    Returns: 64-character SHA256 hex digest
    """
```

### compute_directive_integrity()

Compute integrity hash for a directive version:

```python
def compute_directive_integrity(
    directive_name: str,
    version: str,
    xml_content: str,
    metadata: Optional[Dict[str, Any]] = None
) -> str:
    """
    Compute deterministic integrity hash for a directive version.

    Returns: 64-character SHA256 hex digest
    """
```

### compute_knowledge_integrity()

Compute integrity hash for a knowledge entry:

```python
def compute_knowledge_integrity(
    id: str,
    version: str,
    content: str,
    metadata: Optional[Dict[str, Any]] = None
) -> str:
    """
    Compute deterministic integrity hash for a knowledge entry.

    Returns: 64-character SHA256 hex digest
    """
```

## The Problem: Change Detection

Without integrity hashing:

```
Tool "my_tool" v1.0.0 - Is it the same as yesterday?
Need to compare: manifest, version, files... error-prone!
```

With integrity hashing:

```
Tool "my_tool" v1.0.0 integrity: abc123def456...
Change anything? → Different hash! (Guaranteed)
```

## Canonical Hashing

All integrity functions follow the same pattern:

### 1. Build Canonical Payload

Gather all relevant data in a standard structure:

```python
# For tools
payload = {
    "tool_id": "my_tool",
    "version": "1.0.0",
    "manifest": {...},
    "files": [...]
}

# For directives
payload = {
    "directive_name": "research_topic",
    "version": "1.0.0",
    "xml_hash": "...",
    "metadata": {...}
}
```

### 2. Sort Keys

Ensure deterministic ordering:

```python
# Files are sorted by path
files = sorted(files, key=lambda f: f.get("path", ""))
```

### 3. Canonical JSON

Serialize with no extra whitespace, sorted keys:

```python
canonical = json.dumps(
    payload,
    sort_keys=True,
    separators=(",", ":")
)
# Result: {"a":1,"b":2} (no spaces, sorted keys)
```

### 4. SHA256 Hash

Compute final hash:

```python
hash = hashlib.sha256(canonical.encode()).hexdigest()
# Result: "abc123def456..." (64 hex characters)
```

## Determinism Guarantee

**Same input always produces same hash:**

```python
# Day 1
hash1 = compute_tool_integrity(
    tool_id="my_tool",
    version="1.0.0",
    manifest={"key": "value"},
    files=[]
)

# Day 30 (same input)
hash2 = compute_tool_integrity(
    tool_id="my_tool",
    version="1.0.0",
    manifest={"key": "value"},
    files=[]
)

assert hash1 == hash2  # Always true
```

**Any change produces different hash:**

```python
# Different tool_id
hash_a = compute_tool_integrity("tool_a", "1.0.0", {...})
hash_b = compute_tool_integrity("tool_b", "1.0.0", {...})
assert hash_a != hash_b

# Different version
hash_1 = compute_tool_integrity("tool", "1.0.0", {...})
hash_2 = compute_tool_integrity("tool", "1.1.0", {...})
assert hash_1 != hash_2

# Different manifest
hash_1 = compute_tool_integrity("tool", "1.0.0", {"a": 1})
hash_2 = compute_tool_integrity("tool", "1.0.0", {"a": 2})
assert hash_1 != hash_2
```

## Usage Examples

### Tool Integrity

```python
from lilux.primitives import compute_tool_integrity

tool = {
    "tool_id": "csv_reader",
    "version": "1.2.3",
    "manifest": {
        "executor": "subprocess",
        "config": {
            "command": "python",
            "args": ["reader.py"]
        }
    },
    "files": [
        {"path": "reader.py", "sha256": "abc123..."},
        {"path": "requirements.txt", "sha256": "def456..."}
    ]
}

integrity = compute_tool_integrity(
    tool_id=tool["tool_id"],
    version=tool["version"],
    manifest=tool["manifest"],
    files=tool["files"]
)

print(f"Tool integrity: {integrity}")
# Tool integrity: abc123def456abc123def456abc123def456abc123def456abc123def456abcd
```

### Directive Integrity

```python
from lilux.primitives import compute_directive_integrity

directive_xml = """
<directive name="research_topic" version="1.0.0">
  <metadata>
    <description>Research a topic</description>
  </metadata>
  <process>
    <step>Search</step>
    <step>Analyze</step>
  </process>
</directive>
"""

integrity = compute_directive_integrity(
    directive_name="research_topic",
    version="1.0.0",
    xml_content=directive_xml,
    metadata={"category": "research"}
)

print(f"Directive integrity: {integrity}")
```

### Knowledge Integrity

```python
from lilux.primitives import compute_knowledge_integrity

knowledge_content = """
# API Patterns

## Overview
REST APIs use these patterns...

## Best Practices
1. Use standard HTTP verbs
2. Return JSON
...
"""

integrity = compute_knowledge_integrity(
    id="20260130-api-patterns",
    version="1.0.0",
    content=knowledge_content,
    metadata={"entry_type": "pattern", "category": "api"}
)

print(f"Knowledge integrity: {integrity}")
```

## Use Cases

### 1. Change Detection

```python
# Store original integrity
original_integrity = compute_tool_integrity(...)

# ... time passes ...

# Check if tool changed
new_integrity = compute_tool_integrity(...)
if original_integrity != new_integrity:
    print("Tool has been modified!")
else:
    print("Tool unchanged")
```

### 2. Content-Addressed Storage

```python
# Use integrity as storage key
integrity = compute_tool_integrity(...)
storage_key = f"tools/{integrity}"

# Store at that key
store(storage_key, tool_content)

# Later, retrieve by integrity
content = retrieve(storage_key)
```

### 3. Versioning

```python
# Track versions by integrity
versions = {
    "v1.0.0": "abc123...",  # Original
    "v1.1.0": "def456...",  # Updated
    "v1.1.1": "ghi789...",  # Patched
}

# Check if v1.0.0 still exists
if compute_tool_integrity(...) == versions["v1.0.0"]:
    print("Original still available")
```

### 4. Lockfile Verification

```python
# Load lockfile
lockfile = load_lockfile("my_tool.lock")

# Get tool from registry
tool = get_tool("my_tool", "1.0.0")

# Verify integrity matches
computed = compute_tool_integrity(...)
stored = lockfile.root.integrity

if computed != stored:
    raise ValueError("Tool has been modified!")
```

## Architecture Role

Integrity functions are part of the **verification and reproducibility layer**:

1. **Change detection** - Know when content changed
2. **Content addressing** - Use hash as unique ID
3. **Verification** - Ensure integrity hasn't changed
4. **Lockfile support** - Required for lockfile verification

## Usage

Integrity helpers provide cryptographic hash and signature utilities for tool verification.

````

See `[[lilux/primitives/lockfile]]` for lockfile integration.

## Hash Algorithm Choice

**Why SHA256?**

1. **Standard** - Widely adopted (TLS, Git, etc.)
2. **Fast** - Computed in milliseconds
3. **Collision-free** - For practical purposes
4. **Canonical** - Same payload → same hash
5. **Hex output** - Easy to display and compare

## How Lockfiles Use Integrity Hashes

Integrity hashes in lockfiles serve two purposes:

1. **Change Detection**
   - Recompute hash before execution
   - If hash differs → tool modified since lockfile created
   - Orchestrator must regenerate lockfile

2. **Security**
   - Verify tool hasn't been tampered with
   - If hash differs AND file exists → potential security issue
   - Orchestrator must alert user

### Verification Workflow (Orchestrator Responsibility)

```python
# Orchestrator verifies lockfile before execution
lockfile = load_lockfile("my_tool@1.0.0.lock.json")

for entry in lockfile.resolved_chain:
    # Recompute integrity hash
    current_hash = compute_tool_integrity(
        tool_id=entry["item_id"],
        version=entry["version"],
        manifest=load_manifest(entry["item_id"]),
        files=entry["files"]
    )

    # Compare
    if current_hash != entry["integrity"]:
        # Handle mismatch
        raise IntegrityError(
            f"Tool {entry['item_id']} integrity mismatch!"
        )

# All verified - safe to execute
````

**Note:** Integrity verification (checking hashes, caching results) is an orchestrator responsibility. Lilux only provides the hash computation functions.

## Testing

```python
import pytest
from lilux.primitives import compute_tool_integrity,

def test_tool_integrity_deterministic():
    """Same input produces same hash."""
    payload = {
        "tool_id": "test",
        "version": "1.0.0",
        "manifest": {"a": 1},
        "files": []
    }

    hash1 = compute_tool_integrity(**payload)
    hash2 = compute_tool_integrity(**payload)

    assert hash1 == hash2
    assert len(hash1) == 64  # SHA256 hex

def test_tool_integrity_changes():
    """Different input produces different hash."""
    base = {
        "tool_id": "test",
        "version": "1.0.0",
        "manifest": {"a": 1},
        "files": []
    }

    hash1 = compute_tool_integrity(**base)

    # Change version
    modified = base.copy()
    modified["version"] = "1.1.0"
    hash2 = compute_tool_integrity(**modified)

    assert hash1 != hash2
```

## Limitations and Design

### By Design (Not a Bug)

1. **No partial hashing**
   - Must provide complete payload
   - Ensures all changes detected

2. **No streaming**
   - Loads entire content into memory
   - For large files, pre-hash them

3. **Hex output only**
   - No base64 or custom encoding

4. **No versioning**
   - Always SHA256
   - Future: could support SHA256, SHA512, etc.

## Next Steps

- See lockfile: `[[lilux/primitives/lockfile]]`
- See runtime services: `[[lilux/runtime-services/overview]]`

**Note:** Chain validation and integrity verification with caching are handled by the orchestrator (e.g., RYE), not Lilux. Lilux provides only the pure hash/crypto functions.
