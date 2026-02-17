**Source:** Original implementation: `.ai/tools/rye/examples/` in kiwi-mcp

# Examples Category

## Purpose

Example tools serve as **reference implementations** showing RYE capabilities and best practices.

**Location:** `.ai/tools/rye/examples/`  
**Count:** 2 tools  
**Executor:** Varies

## Core Example Tools

### 1. Git Status (`git_status.py`)

**Purpose:** Demonstrate git operations

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "examples"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "repo": {"type": "string", "description": "Repository path", "default": "."},
        "detailed": {"type": "boolean", "default": False},
    },
}

def main(repo: str = ".", detailed: bool = False) -> dict:
    """Show git repository status."""
    import subprocess
    
    result = subprocess.run(
        ["git", "-C", repo, "status", "--porcelain"],
        capture_output=True,
        text=True
    )
    
    return {
        "status": "success",
        "repo": repo,
        "output": result.stdout,
        "files_changed": len(result.stdout.strip().split("\n")) if result.stdout.strip() else 0,
    }
```

**Learning Points:**
- Using `python_runtime` executor
- Subprocess integration
- Config schema definition
- Return format

### 2. Health Check (`health_check.py`)

**Purpose:** Demonstrate system diagnostics

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "examples"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "checks": {
            "type": "array",
            "items": {"type": "string", "enum": ["disk", "memory", "cpu"]},
            "default": ["disk", "memory", "cpu"]
        },
        "verbose": {"type": "boolean", "default": False},
    },
}

def main(checks: list = None, verbose: bool = False) -> dict:
    """Perform system health check."""
    import psutil
    
    checks = checks or ["disk", "memory", "cpu"]
    results = {"status": "healthy", "checks": {}}
    
    if "disk" in checks:
        disk = psutil.disk_usage("/")
        results["checks"]["disk"] = {
            "used_percent": disk.percent,
            "status": "warning" if disk.percent > 80 else "ok"
        }
    
    if "memory" in checks:
        memory = psutil.virtual_memory()
        results["checks"]["memory"] = {
            "used_percent": memory.percent,
            "status": "warning" if memory.percent > 80 else "ok"
        }
    
    if "cpu" in checks:
        cpu = psutil.cpu_percent(interval=1)
        results["checks"]["cpu"] = {
            "used_percent": cpu,
            "status": "warning" if cpu > 80 else "ok"
        }
    
    # Overall status
    for check in results["checks"].values():
        if check["status"] != "ok":
            results["status"] = "warning"
    
    return results
```

**Learning Points:**
- System diagnostics
- Conditional checks
- Status aggregation
- Error handling

## Metadata Pattern

All example tools follow this pattern:

```python
# .ai/tools/rye/examples/{name}.py

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "examples"

CONFIG_SCHEMA = { ... }

def main(**kwargs) -> dict:
    """Example implementation with documentation."""
    pass
```

## Learning from Examples

Each example demonstrates:

1. **Tool Definition**
   - Metadata (`__version__`, `__tool_type__`, etc.)
   - CONFIG_SCHEMA definition
   - Function signature

2. **Execution Pattern**
   - How executor routes to runtime
   - Environment resolution
   - Subprocess/library calls

3. **Return Format**
   - JSON-serializable output
   - Error handling
   - Status reporting

4. **Best Practices**
   - Documentation
   - Input validation
   - Resource cleanup

## Using Examples as Templates

### Step 1: Copy Example

```bash
cp .ai/tools/rye/examples/git_status.py \
   .ai/tools/myproject/my_git_tool.py
```

### Step 2: Customize

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "myproject"  # Change category

CONFIG_SCHEMA = {
    # Define your parameters
}

def main(**kwargs) -> dict:
    # Implement your logic
    pass
```

### Step 3: Test

```bash
Call my_git_tool with:
  repo: "/path/to/repo"
```

## Example Tool Gallery

| Tool | Purpose | Learning | Complexity |
|------|---------|----------|------------|
| `git_status.py` | Git operations | Subprocess, CLI | Low |
| `health_check.py` | System diagnostics | Metrics, aggregation | Medium |

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Count** | 2 tools |
| **Location** | `.ai/tools/rye/examples/` |
| **Executor** | python_runtime |
| **Purpose** | Reference implementations |
| **Use Cases** | Learning, templates, demos |

## Related Documentation

- [overview](overview.md) - All categories
- [core/extractors](../core/extractors.md) - Schema-driven extraction
- [../bundle/structure](../bundle/structure.md) - Bundle organization
