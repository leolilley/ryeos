# Registry Category

## Purpose

Registry tool provides **tool distribution and package management** functionality. It's a data-driven tool that uses HTTP client primitive to communicate with remote registry.

**Location:** `.ai/tools/rye/registry/registry.py`
**Count:** 1 tool
**Executor:** `http_client`

**Implementation Status:** ✅ Already implemented in RYE

**Note on `__protected__` Field:** The `__protected__ = True` metadata field in the registry tool is **NOT related** to tool spaces or shadowing. It indicates that the registry tool itself should not be overridden. This is **unrelated** to the core tool spaces model (project/user/system). See [[../../executor/tool-resolution-and-validation.md]] for the correct tool spaces model.

---

## How to Access the Registry Tool

The registry tool is accessed via `mcp__rye__execute` as a regular data-driven tool:

```json
{
  "item_type": "tool",
  "item_id": "registry",
  "project_path": "/path/to/project",
  "parameters": {
    "operation": "publish|pull|search|auth|key",
    "package": "tool-name",
    "version": "1.0.0",
    "registry": "https://registry.rye-lilux.dev"
  }
}
```

This means:
- **To search the registry:** Use `execute` with `item_id="registry"` and `operation="search"`
- **To download from the registry:** Use `execute` with `item_id="registry"` and `operation="pull"`
- **To publish to the registry:** Use `execute` with `item_id="registry"` and `operation="publish"`

The registry is **not** accessible through `search`, `load`, `sign`, or `help` - it is only accessible through `execute` as a regular data-driven tool.

---

## Important: Registry is NOT Used for Local Tool Discovery

**Registry is ONLY for:** sharing, publishing, and remote distribution

**Local tool discovery uses FILESYSTEM-BASED discovery only:**
- All tools (RYE bundled + user custom) discovered from `.ai/tools/` filesystem
- No registry access required for local development or execution
- Registry is optional for publishing tools to share with others

See [rye/principles](../rye/principles.md) for on-demand loading architecture.

---

## Note: Registry is Optional for Tool Distribution

**Primary Tool Discovery:** Filesystem-based (from `.ai/tools/`)
**Optional Registry:** For sharing and publishing tools

| Operation | Filesystem Discovery | Registry |
|-----------|---------------------|-----------|
| **Local development** | ✅ Required | ❌ Not used |
| **Local execution** | ✅ Required | ❌ Not used |
| **Testing tools** | ✅ Required | ❌ Not used |
| **Publishing tools** | ❌ Not needed | ✅ Available |
| **Pulling shared tools** | ❌ Not needed | ✅ Available |
| **Searching public tools** | ❌ Not needed | ✅ Available |

**Bottom Line:** Registry is optional for sharing. Local development and execution works entirely from filesystem.

See [rye/principles](../rye/principles.md) for on-demand loading architecture.

---

## Registry Tool Definition

### Core Registry Tool

```python
__tool_type__ = "python"
__executor_id__ = "http_client"  # Uses HTTP for remote operations
__category__ = "registry"
__version__ = "1.0.0"
__protected__ = True  # Protected tool - do not override

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {
            "type": "string",
            "enum": ["publish", "pull", "search", "auth", "key"]
        },
        "package": {"type": "string", "description": "Package name"},
        "version": {"type": "string", "description": "Package version (semver)"},
        "registry": {"type": "string", "description": "Registry endpoint"},
    },
    "required": ["operation", "registry"]
}
```

## Operations

### Publish

**Push tool to registry**

```bash
Call registry with:
  operation: "publish"
  package: "my-tool"
  version: "1.0.0"
  registry: "https://registry.rye-lilux.dev"
```

**Process:**
1. Validate tool format
2. Create package bundle
3. Sign with credentials
4. Upload to registry
5. Update registry index

### Pull

**Fetch tool from registry**

```bash
Call registry with:
  operation: "pull"
  package: "my-tool"
  version: "1.0.0"
  registry: "https://registry.rye-lilux.dev"
```

**Process:**
1. Query registry for package
2. Verify signatures
3. Download tool
4. Install to .ai/tools/
5. Update local registry

### Search

**Search registry for tools**

```bash
Call registry with:
  operation: "search"
  package: "git*"
  registry: "https://registry.rye-lilux.dev"
```

**Returns:**
```json
{
  "results": [
    {
      "name": "git",
      "version": "1.0.0",
      "author": "rye-team",
      "description": "Git operations",
      "downloads": 1234
    },
    {
      "name": "git-advanced",
      "version": "2.0.0",
      "author": "community",
      "description": "Advanced git features",
      "downloads": 456
    }
  ]
}
```

### Auth

**Manage registry authentication**

```bash
Call registry with:
  operation: "auth"
  action: "login"
  registry: "https://registry.rye-lilux.dev"
  username: "user@example.com"
```

**Actions:**
- `login` - Authenticate with registry
- `logout` - Remove credentials
- `whoami` - Show current user
- `token` - Manage API tokens

### Key

**Manage registry keys**

```bash
Call registry with:
  operation: "key"
  action: "create"
  name: "signing-key"
  registry: "https://registry.rye-lilux.dev"
```

**Actions:**
- `create` - Generate new key
- `list` - List all keys
- `delete` - Remove key
- `export` - Export public key
- ```

---

## Registry Architecture

```
Local .ai/tools/
    │
    ├─→ registry pull
    │   └─→ Remote Registry
    │       ├─ Package index
    │       ├─ Package files
    │       └─ Signatures
    │
    ├─→ registry push
    │   └─→ Remote Registry
    │       ├─ Upload files
    │       ├─ Sign package
    │       └─ Update index
    │
    └─→ registry search
        └─→ Remote Registry
            └─ Query index
```

## Package Metadata

Tools in registry include:

```yaml
name: git
version: 1.0.0
description: Git operations tool
author: rye-team
license: MIT

tool_type: python
executor_id: python_runtime
category: capabilities

config_schema: {...}
env_config: {...}

signatures:
  sha256: "..."
  pgp: "..."

dependencies:
  - subprocess:>=1.0.0
  - python_runtime:>=2.0.0
```

## Registry Endpoints

### Public Registry

```
https://registry.rye-lilux.dev
```

Discover and install community tools

### Private Registry

```
https://registry.mycompany.com
```

Host internal tools

### Local Registry

```
file:///home/user/.local/rye-registry
```

Offline package management

## Metadata Pattern

Registry is a single special tool:

```python
# .ai/tools/rye/registry/registry.py

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "http_client"  # Remote operations
__category__ = "registry"

CONFIG_SCHEMA = { ... }

def main(**kwargs) -> dict:
    """Registry operations."""
    pass
```

## Usage Examples

### Publish Custom Tool

```bash
Call registry with:
  operation: "publish"
  package: "my-custom-tool"
  version: "1.0.0"
  registry: "https://registry.rye-lilux.dev"
```

### Install Tool from Registry

```bash
Call registry with:
  operation: "pull"
  package: "community-tool"
  version: "2.0.0"
  registry: "https://registry.rye-lilux.dev"
```

### Search for Tools

```bash
Call registry with:
  operation: "search"
  package: "*database*"
  registry: "https://registry.rye-lilux.dev"
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Count** | 1 tool |
| **Location** | `.ai/tools/rye/registry/` |
| **Executor** | `http_client` |
| **Purpose** | Tool distribution & management |
| **Operations** | publish, pull, search, auth, key |
| **Remote** | HTTP-based registry endpoints |

## Related Documentation

- [overview](../overview.md) - All categories
- [../bundle/structure](../bundle/structure.md) - Bundle organization
- [../executor/routing](../executor/routing.md) - How HTTP executor works
