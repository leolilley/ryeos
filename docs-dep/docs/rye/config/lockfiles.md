# Lockfile Configuration

## Overview

The orchestrator manages lockfile resolution with a three-tier architecture. It reads configuration, resolves paths with precedence, and passes explicit paths to Lilux.

## Key Principle

> **The orchestrator resolves all paths and passes a SINGLE explicit path to Lilux. Lilux never does path discovery or precedence logic.**

---

## Three-Tier Architecture

### Tier 1: System (Bundled, Read-Only)

```
site-packages/{package}/.ai/lockfiles/
├── subprocess@1.0.0.lock.json
├── http_client@1.0.0.lock.json
└── ...
```

- Pre-validated lockfiles bundled with pip package
- Read-only (installed via pip)
- **Lowest precedence**

### Tier 2: User (Default, Read-Write)

```
~/.ai/lockfiles/
├── my_tool@1.0.0.lock.json
└── ...
```

- Default location for user-created lockfiles
- Shared across all projects
- **Medium precedence**

### Tier 3: Project (Opt-In, Read-Write)

```
{project}/lockfiles/
├── custom_tool@1.0.0.lock.json
└── ...
```

- Only used when `scope: project` in config
- Project-specific lockfiles
- **Highest precedence**

---

## Configuration

```yaml
# ~/.ai/config/config.yaml

lockfiles:
  scope: user  # Options: "user" (default), "project"
```

| Scope | Read Precedence | Write Location |
|-------|-----------------|----------------|
| `user` (default) | project → user → system | `~/.ai/lockfiles/` |
| `project` | project → user → system | `{project}/lockfiles/` |

---

## Resolution Flow

### Reading a Lockfile

The orchestrator checks directories in precedence order and returns the first match:

```python
class LockfileResolver:
    """Resolves lockfile paths with precedence."""
    
    def __init__(
        self,
        system_dir: Path,
        user_dir: Path,
        project_dir: Optional[Path] = None,
        scope: str = "user"
    ):
        self.system_dir = system_dir
        self.user_dir = user_dir
        self.project_dir = project_dir
        self.scope = scope
        self.manager = LockfileManager()  # Lilux primitives
    
    def get_lockfile(self, tool_id: str, version: str) -> Optional[Lockfile]:
        """
        Find and load lockfile using precedence.
        
        Checks: project → user → system
        Returns first match, or None.
        """
        path = self._resolve_read_path(tool_id, version)
        if path:
            return self.manager.load(path)
        return None
    
    def _resolve_read_path(self, tool_id: str, version: str) -> Optional[Path]:
        """Apply precedence: project → user → system."""
        name = f"{tool_id}@{version}.lock.json"
        
        candidates = [
            self.project_dir,  # Highest precedence
            self.user_dir,
            self.system_dir,   # Lowest precedence
        ]
        
        for dir in candidates:
            if dir and (dir / name).exists():
                return dir / name
        
        return None
```

### Writing a Lockfile

Write location depends on configured scope:

```python
def save_lockfile(self, lockfile: Lockfile) -> Path:
    """Save lockfile to appropriate location based on scope."""
    path = self._resolve_write_path(
        lockfile.root.tool_id,
        lockfile.root.version
    )
    return self.manager.save(lockfile, path)

def _resolve_write_path(self, tool_id: str, version: str) -> Path:
    """Determine write location from scope."""
    name = f"{tool_id}@{version}.lock.json"
    
    if self.scope == "project" and self.project_dir:
        return self.project_dir / name
    
    return self.user_dir / name
```

---

## LockfileConfig

Reads user configuration and determines paths:

```python
import os
import yaml
from pathlib import Path
from typing import Optional, Dict, Any


class LockfileConfig:
    """
    Reads lockfile configuration and determines directories.
    
    This is orchestrator logic - Lilux never reads config.
    """
    
    def __init__(self, user_space: Optional[Path] = None):
        if user_space:
            self.user_space = Path(user_space)
        else:
            user_space_env = os.getenv("USER_SPACE", str(Path.home() / ".ai"))
            self.user_space = Path(user_space_env)
        
        self.config_file = self.user_space / "config" / "config.yaml"
        self.config = self._read_config()
    
    def _read_config(self) -> Dict[str, Any]:
        """Read config file, return empty dict if missing."""
        if not self.config_file.exists():
            return {}
        
        try:
            with open(self.config_file) as f:
                return yaml.safe_load(f) or {}
        except Exception:
            return {}
    
    def get_lockfile_scope(self) -> str:
        """Get scope: 'user' (default) or 'project'."""
        return self.config.get("lockfiles", {}).get("scope", "user")
    
    def get_system_lockfile_dir(self) -> Path:
        """Get bundled lockfile directory."""
        import rye
        return Path(rye.__file__).parent / ".ai" / "lockfiles"
    
    def get_user_lockfile_dir(self) -> Path:
        """Get user lockfile directory."""
        return self.user_space / "lockfiles"
    
    def get_project_lockfile_dir(self, project_path: Path) -> Optional[Path]:
        """Get project lockfile directory (only if scope=project)."""
        if self.get_lockfile_scope() == "project":
            return project_path / "lockfiles"
        return None
```

---

## Complete Example

```python
from pathlib import Path
from rye.config.lockfile_config import LockfileConfig
from rye.lockfile_resolver import LockfileResolver

# Initialize config
config = LockfileConfig()

# Create resolver with all tier directories
resolver = LockfileResolver(
    system_dir=config.get_system_lockfile_dir(),
    user_dir=config.get_user_lockfile_dir(),
    project_dir=config.get_project_lockfile_dir(Path.cwd()),
    scope=config.get_lockfile_scope()
)

# Get lockfile (resolves precedence, passes path to Lilux)
lockfile = resolver.get_lockfile("my_tool", "1.0.0")

if lockfile:
    print(f"Found lockfile for {lockfile.root.tool_id}")
else:
    print("No lockfile found - will create one")
    
    # Create and save new lockfile
    chain = resolve_tool_chain("my_tool", "1.0.0")
    new_lockfile = resolver.create_lockfile(
        root_tool={"tool_id": "my_tool", "version": "1.0.0"},
        resolved_chain=chain
    )
    resolver.save_lockfile(new_lockfile)
```

---

## User Experience

### Default Setup (scope: user)

```bash
# Execute tool - lockfile saved to ~/.ai/lockfiles/
rye execute my_tool

# Lockfile created at: ~/.ai/lockfiles/my_tool@1.0.0.lock.json
# Shared across all projects
```

### Project Setup (scope: project)

```yaml
# ~/.ai/config/config.yaml
lockfiles:
  scope: project
```

```bash
cd /my-project

# Execute tool - lockfile saved to project
rye execute my_tool

# Lockfile created at: /my-project/lockfiles/my_tool@1.0.0.lock.json
# Project-specific
```

---

## Git Tracking

### User Lockfiles (`~/.ai/lockfiles/`)

- Not git-tracked (in user's home directory)
- Personal verification cache
- Shared across projects

### Project Lockfiles (`{project}/lockfiles/`)

- User decides whether to git-track
- Can add to `.gitignore` if desired
- Useful for pinning project-specific tool versions

```bash
# To track project lockfiles
git add lockfiles/
git commit -m "Pin tool versions"

# To ignore
echo "lockfiles/" >> .gitignore
```

---

## Package Distribution

Bundled lockfiles are included in the pip package:

```toml
# pyproject.toml
[tool.setuptools.package-data]
rye = [".ai/lockfiles/**/*.lock.json"]
```

```
# MANIFEST.in
recursive-include rye/.ai/lockfiles *.lock.json
```

**Why bundling is safe:**
- Tools are hash-validated
- Lockfiles contain expected hashes
- Any mismatch = execution fails
- pip installation is atomic

---

## Separation of Concerns

| Concern | Owner |
|---------|-------|
| Config reading | Orchestrator (LockfileConfig) |
| Path resolution | Orchestrator (LockfileResolver) |
| Tier precedence | Orchestrator (LockfileResolver) |
| Directory conventions | Orchestrator |
| Lockfile I/O | Lilux (LockfileManager) |
| Validation | Orchestrator (RYE) |
| Lockfile creation | Orchestrator (RYE) |
| Integrity hashing | Lilux |

---

## See Also

- **Lilux Lockfile I/O:** `[lilux/primitives/lockfile](../lilux/primitives/lockfile.md)`
- **Config Overview:** `[rye/config/overview](overview.md)`
