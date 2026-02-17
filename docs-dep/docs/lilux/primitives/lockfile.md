# Lockfile Primitive

## Purpose

Provide lockfile I/O and data structures for reproducible tool execution. Lilux handles lockfile storage and retrieval at explicit paths provided by the orchestrator.

## Key Principle

> **Lilux receives explicit paths. It never resolves paths, reads config, or applies precedence logic.**

```
┌─────────────────────────────────────────┐
│  Orchestrator                           │
│                                         │
│  1. Reads config                        │
│  2. Resolves precedence                 │
│  3. Passes SINGLE path to Lilux         │
└─────────────┬───────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────┐
│  Lilux LockfileManager                  │
│                                         │
│  1. Receives explicit path              │
│  2. Loads/saves lockfile                │
│  3. NO config reading                   │
│  4. NO path discovery                   │
│  5. NO precedence logic                 │
└─────────────────────────────────────────┘
```

---

## Data Structures

### Lockfile

```python
from dataclasses import dataclass
from typing import Optional, List

@dataclass
class LockfileRoot:
    """Root tool in lockfile."""
    tool_id: str
    version: str
    integrity: str

@dataclass
class Lockfile:
    """Lockfile data structure."""
    lockfile_version: int
    generated_at: str
    root: LockfileRoot
    resolved_chain: List[dict]
    registry: Optional[dict] = None
```

---

## API

### LockfileManager

```python
from pathlib import Path

class LockfileManager:
    """
    Lockfile I/O with explicit paths.
    
    This class does NOT:
    - Read configuration files
    - Resolve paths or apply precedence
    - Know about directory conventions
    
    The orchestrator handles all path resolution.
    """
    
    def load(self, path: Path) -> Lockfile:
        """
        Load a lockfile from an explicit path.
        
        Args:
            path: Full path to lockfile (provided by orchestrator)
        
        Returns:
            Parsed Lockfile object
        
        Raises:
            FileNotFoundError: If lockfile doesn't exist
            ValueError: If lockfile is malformed
        """
        ...
    
    def save(self, lockfile: Lockfile, path: Path) -> Path:
        """
        Save a lockfile to an explicit path.
        
        Args:
            lockfile: Lockfile object to save
            path: Full path where to save (provided by orchestrator)
        
        Returns:
            Path where lockfile was saved
        
        Raises:
            FileNotFoundError: If parent directory doesn't exist
        
        Note:
            LockfileManager does NOT create parent directories.
            The orchestrator must ensure the directory exists.
        """
        ...
    
    def exists(self, path: Path) -> bool:
        """Check if lockfile exists at path."""
        return path.exists()
```

---

## Usage

### Loading a Lockfile

```python
from lilux.primitives.lockfile import LockfileManager

manager = LockfileManager()

# Orchestrator provides the resolved path
lockfile_path = Path("/path/to/lockfiles/my_tool@1.0.0.lock.json")

if manager.exists(lockfile_path):
    lockfile = manager.load(lockfile_path)
    print(f"Tool: {lockfile.root.tool_id}")
    print(f"Version: {lockfile.root.version}")
    print(f"Chain: {len(lockfile.resolved_chain)} entries")
```

### Saving a Lockfile

```python
from lilux.primitives.lockfile import LockfileManager, Lockfile, LockfileRoot

manager = LockfileManager()

# Create lockfile
lockfile = Lockfile(
    lockfile_version=1,
    generated_at="2026-01-30T12:00:00Z",
    root=LockfileRoot(
        tool_id="my_tool",
        version="1.0.0",
        integrity="abc123def456..."
    ),
    resolved_chain=[
        {"tool_id": "subprocess", "version": "1.0.0", "integrity": "..."}
    ]
)

# Orchestrator provides the write path
write_path = Path("/path/to/lockfiles/my_tool@1.0.0.lock.json")
saved_path = manager.save(lockfile, write_path)
```

---

## Lockfile Format

```json
{
  "lockfile_version": 1,
  "generated_at": "2026-01-30T12:00:00Z",
  "root": {
    "tool_id": "my_tool",
    "version": "1.0.0",
    "integrity": "abc123def456..."
  },
  "resolved_chain": [
    {
      "tool_id": "subprocess",
      "version": "1.0.0",
      "integrity": "def789ghi012...",
      "executor": "subprocess"
    }
  ],
  "registry": {
    "url": "https://registry.example.com",
    "fetched_at": "2026-01-30T12:00:00Z"
  }
}
```

### Fields

| Field | Type | Description |
|-------|------|-------------|
| `lockfile_version` | int | Format version (currently 1) |
| `generated_at` | string | ISO timestamp when created |
| `root` | object | Top-level tool being locked |
| `resolved_chain` | array | Ordered list of tool dependencies |
| `registry` | object | Optional provenance information |

---

## Design Principles

### 1. Explicit Paths Only

Lilux receives complete file paths as parameters:

```python
# ✅ Correct - explicit path from orchestrator
manager.load(Path("/path/to/lockfiles/tool@1.0.0.lock.json"))

# ❌ Wrong - Lilux doesn't resolve paths
manager.load(tool_id="tool", version="1.0.0")  # Not supported
```

### 2. I/O Only - No Validation

LockfileManager only handles I/O:
- Load lockfile from disk
- Save lockfile to disk
- Check if file exists

Validation and creation logic belongs in the orchestrator (see below).

### 3. No Configuration Reading

Lilux does not:
- Read configuration files
- Check environment variables for paths
- Have default directories

### 4. Separation of Concerns

| Concern | Responsibility |
|---------|----------------|
| Path resolution | Orchestrator |
| Precedence logic | Orchestrator |
| Config reading | Orchestrator |
| Lockfile validation | Orchestrator |
| Lockfile creation | Orchestrator |
| **Lockfile I/O** | **Lilux** |
| **Format parsing** | **Lilux** |

---

## Integration with Orchestrator

The orchestrator (e.g., RYE) handles higher-level operations:

```python
# In orchestrator code (not Lilux)
from lilux.primitives.lockfile import LockfileManager, Lockfile, LockfileRoot
from rye.executor.integrity_verifier import IntegrityVerifier

manager = LockfileManager()
verifier = IntegrityVerifier()

def execute_with_lockfile(tool_id: str, version: str, lockfile_path: Path):
    """Orchestrator handles validation and creation."""
    
    if manager.exists(lockfile_path):
        # Load
        lockfile = manager.load(lockfile_path)
        
        # Validate (orchestrator logic)
        result = verifier.verify_chain(lockfile.resolved_chain)
        
        if not result.success:
            # Regenerate (orchestrator logic)
            lockfile = create_lockfile(tool_id, version)
            manager.save(lockfile, lockfile_path)
    else:
        # Create (orchestrator logic)
        lockfile = create_lockfile(tool_id, version)
        manager.save(lockfile, lockfile_path)
    
    # Execute
    execute_chain(lockfile.resolved_chain)
```

---

## Error Handling

```python
from lilux.primitives.lockfile import LockfileManager
from lilux.primitives.errors import LockfileError

manager = LockfileManager()

try:
    lockfile = manager.load(path)
except FileNotFoundError:
    print("Lockfile not found")
except LockfileError as e:
    print(f"Malformed lockfile: {e}")
```

---

## Testing

```python
import pytest
from pathlib import Path
from lilux.primitives.lockfile import LockfileManager, Lockfile, LockfileRoot

def test_save_and_load(tmp_path):
    manager = LockfileManager()
    
    lockfile = Lockfile(
        lockfile_version=1,
        generated_at="2026-01-30T12:00:00Z",
        root=LockfileRoot(
            tool_id="test",
            version="1.0.0",
            integrity="abc123"
        ),
        resolved_chain=[]
    )
    
    path = tmp_path / "test@1.0.0.lock.json"
    manager.save(lockfile, path)
    
    loaded = manager.load(path)
    assert loaded.root.tool_id == "test"
    assert loaded.root.version == "1.0.0"

def test_load_nonexistent():
    manager = LockfileManager()
    
    with pytest.raises(FileNotFoundError):
        manager.load(Path("/nonexistent/path.lock.json"))
```

---

## See Also

- **Integrity Helpers:** `[[lilux/primitives/integrity]]`
- **Runtime Services Overview:** `[[lilux/runtime-services/overview]]`
