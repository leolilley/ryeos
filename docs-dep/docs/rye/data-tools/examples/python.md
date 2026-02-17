**Source:** Original implementation: `.ai/tools/rye/python/lib/` and `.ai/tools/python/lib/` in kiwi-mcp

# Python Category

## Purpose

The Python category provides **shared Python libraries** that can be imported by tools.

**Location:** 
- Bundled: `.ai/tools/rye/python/lib/` (included with RYE)
- User: `.ai/tools/python/lib/` (user-created)

**Type:** Libraries (not executable tools)

## Key Difference

| Aspect | Tools | Libraries |
|--------|-------|-----------|
| **Executable** | ✓ Yes | ✗ No |
| `__executor_id__` | "python_runtime", "subprocess", etc. | None/absent |
| `main()` function | ✓ Required | ✗ Absent |
| **Purpose** | Standalone execution | Code reuse |
| **Import** | Via MCP tools | Direct import in tools |

## Library Structure

```python
# .ai/tools/rye/python/lib/proxy_pool.py

__tool_type__ = "python_lib"
# No __executor_id__ (not executable)
# No main() function

class ProxyPool:
    """Shared proxy pool implementation."""
    
    def __init__(self, proxies: list):
        self.proxies = proxies
        self.current_index = 0
    
    def get_proxy(self) -> str:
        """Get next proxy in rotation."""
        proxy = self.proxies[self.current_index]
        self.current_index = (self.current_index + 1) % len(self.proxies)
        return proxy
    
    def add_proxy(self, proxy: str):
        """Add proxy to pool."""
        self.proxies.append(proxy)
    
    def remove_proxy(self, proxy: str):
        """Remove proxy from pool."""
        if proxy in self.proxies:
            self.proxies.remove(proxy)
```

## Using Libraries in Tools

### Importing

```python
# .ai/tools/rye/examples/test_proxy_pool.py

__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "examples"

# Import shared library
from rye.tools.rye.python.lib.proxy_pool import ProxyPool

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "proxy_list": {
            "type": "array",
            "items": {"type": "string"}
        },
    },
    "required": ["proxy_list"]
}

def main(proxy_list: list) -> dict:
    """Test proxy pool."""
    pool = ProxyPool(proxy_list)
    
    proxies_tested = []
    for i in range(len(proxy_list)):
        proxy = pool.get_proxy()
        proxies_tested.append(proxy)
    
    return {
        "status": "success",
        "proxy_rotation": proxies_tested,
    }
```

## RYE Bundled Libraries

### Located: `.ai/tools/rye/python/lib/`

| Library | Purpose |
|---------|---------|
| `proxy_pool.py` | Proxy pool management and rotation |
| (others) | Additional shared utilities |

## Shared Library Patterns

### Pattern 1: Utility Classes

```python
# Shared utilities that tools import

class ConnectionPool:
    """Manage connection pooling."""
    def __init__(self, max_connections):
        self.max_connections = max_connections
    
    def get_connection(self):
        pass

class CacheManager:
    """Manage caching."""
    def __init__(self, ttl):
        self.ttl = ttl
    
    def get(self, key):
        pass
```

### Pattern 2: Helper Functions

```python
# Reusable functions

def retry_with_backoff(func, max_retries=3, backoff_factor=2):
    """Retry function with exponential backoff."""
    for attempt in range(max_retries):
        try:
            return func()
        except Exception as e:
            if attempt == max_retries - 1:
                raise
            time.sleep(backoff_factor ** attempt)

def parse_config_file(path):
    """Parse configuration file."""
    pass
```

### Pattern 3: Data Models

```python
# Shared data structures

class ToolConfig:
    """Tool configuration."""
    def __init__(self, name, version, schema):
        self.name = name
        self.version = version
        self.schema = schema
    
    def validate(self, data):
        pass

class ExecutionResult:
    """Result of tool execution."""
    def __init__(self, status, data, error=None):
        self.status = status
        self.data = data
        self.error = error
    
    def to_dict(self):
        return {
            "status": self.status,
            "data": self.data,
            "error": self.error
        }
```

## Metadata Pattern

All Python libraries follow this pattern:

```python
# .ai/tools/{category}/python/lib/{name}.py

__tool_type__ = "python_lib"
# No __executor_id__
# No __category__ (optional)

class MyClass:
    """Shared library class."""
    pass

def my_function():
    """Shared library function."""
    pass
```

## Best Practices

### 1. No Side Effects

```python
# ✓ GOOD: Pure functions
def calculate_hash(data: str) -> str:
    """Calculate SHA256 hash."""
    import hashlib
    return hashlib.sha256(data.encode()).hexdigest()

# ✗ BAD: Global state
_cache = {}  # Don't use global variables

def get_cached(key: str):
    return _cache.get(key)
```

### 2. Clear Dependencies

```python
# ✓ GOOD: Explicit imports
import json
import hashlib

# ✗ BAD: Hidden dependencies
from .* import *  # Avoid wildcards
```

### 3. Comprehensive Docstrings

```python
# ✓ GOOD: Full documentation
class ProxyPool:
    """
    Manage a pool of proxy servers.
    
    Provides round-robin proxy selection with failure handling.
    
    Args:
        proxies: List of proxy URLs
        
    Example:
        >>> pool = ProxyPool(["http://proxy1", "http://proxy2"])
        >>> proxy = pool.get_proxy()
    """
    pass

# ✗ BAD: No documentation
def get_proxy():
    pass
```

### 4. Version Compatibility

```python
# ✓ GOOD: Version-aware libraries
__version__ = "1.0.0"
__compatibility__ = ["python>=3.8", "rye>=1.0.0"]

# ✗ BAD: Version conflicts
# Don't assume specific versions
```

## Bundled vs User Libraries

### Bundled (`.ai/tools/rye/python/lib/`)

- Installed with RYE
- Maintained by RYE team
- Available to all tools
- Well-tested and documented

### User (`.ai/tools/python/lib/`)

- User-created libraries
- User-maintained
- Available to user's tools
- Can extend bundled libraries

## Import Paths

### Bundled Libraries

```python
# Import from bundled library
from rye.tools.rye.python.lib.proxy_pool import ProxyPool
```

### User Libraries

```python
# Import from user library
from user.tools.python.lib.my_lib import MyClass
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Type** | Python libraries |
| **Location** | `.ai/tools/rye/python/lib/` (bundled) or `.ai/tools/python/lib/` (user) |
| **Executable** | No |
| **Metadata** | `__tool_type__ = "python_lib"` only |
| **Purpose** | Code reuse across tools |
| **Import** | Direct import in tool files |

## Related Documentation

- [core/extractors](../core/extractors.md) - Schema-driven extraction
- [overview](overview.md) - All categories
- [../bundle/structure](../bundle/structure.md) - Bundle organization
