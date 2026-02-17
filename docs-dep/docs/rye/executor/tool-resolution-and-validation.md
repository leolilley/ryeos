# Tool Resolution and Validation

## Overview

This document describes how RYE resolves tools across multiple spaces and validates execution chains that span those spaces. It covers:

- Tool spaces and precedence rules
- Tool resolution algorithm
- Cross-space chain validation
- Shadowing behavior and protection rules

---

## Tool Spaces Model

RYE uses a three-space model for tool resolution with explicit precedence:

| Space       | Location                              | Mutability | Precedence |
| ----------- | ------------------------------------- | ---------- | ----------- |
| **Project** | `{project}/.ai/`                    | Mutable    | 1 (highest)  |
| **User**    | `~/.ai/`                            | Mutable    | 2 (medium)   |
| **System**   | `site-packages/rye/.ai/`          | Immutable  | 3 (lowest)   |

### Key Principles

- **Resolution is deterministic**: Always searches project → user → system
- **Shadowing is intentional**: Project/user tools can shadow system tools
- **System is immutable**: Read-only, installed via pip
- **Chains can span spaces**: Tools can depend on tools from other spaces (with validation)

---

## Tool Resolution Algorithm

When a tool is requested, RYE resolves it by precedence:

### Resolution Steps

1. **Search project space** - `{project_path}/.ai/`
2. **If not found, search user space** - `~/.ai/` (or `USER_SPACE`)
3. **If not found, search system space** - `{install_location}/.ai/`
4. **Return first match** with `(path, space)` tuple

### Pseudocode

```python
def resolve_tool(item_id: str, project_path: str) -> Tuple[str, str]:
    """
    Resolve tool path by precedence.

    Returns: (full_path, space)
    """
    search_spaces = [
        (Path(project_path) / ".ai", "project"),
        (Path.home() / ".ai", "user"),
        (get_install_location() / ".ai", "system"),
    ]

    for space_dir, space in search_spaces:
        tool_path = space_dir / "tools" / f"{item_id}.py"
        if tool_path.exists():
            return (tool_path, space)

    raise ToolNotFoundError(item_id)
```

### Example Resolution

```
Request: item_id="git"

1. Search: /home/user/myproject/.ai/tools/git.py → Found!
2. Return: ("/home/user/myproject/.ai/tools/git.py", "project")

---

Request: item_id="bootstrap"

1. Search: /home/user/myproject/.ai/tools/bootstrap.py → Not found
2. Search: ~/.ai/tools/bootstrap.py → Not found
3. Search: /usr/lib/python3.11/site-packages/rye/.ai/tools/bootstrap.py → Found!
4. Return: ("/usr/lib/python3.11/site-packages/rye/.ai/tools/bootstrap.py", "system")
```

---

## Chain Resolution

When a tool delegates to another tool (via `__executor_id__`), RYE resolves each executor in the chain.

### Example Chain

```
Tool: git.py (project space)
  ↓ delegates to
Runtime: python_runtime.py (system space)
  ↓ delegates to
Primitive: subprocess.py (system space)
```

### Chain Resolution Process

1. Load tool metadata from resolved path
2. Extract `__executor_id__`
3. Resolve executor using **same precedence rules** (starting from current tool's space)
4. Repeat until reaching a primitive (`__executor_id__ = None`)
5. Track space of each chain element

### Key Point

Each element in the chain is resolved independently with precedence:

```python
# Project tool delegates to system runtime
git_tool = resolve_tool("git", project="/home/user/myproject")
# Returns: ("/home/user/myproject/.ai/tools/git.py", "project")

python_runtime = resolve_tool("python_runtime", project="/home/user/myproject")
# Returns: ("/usr/lib/python/rye/.ai/tools/python_runtime.py", "system")

subprocess = resolve_tool("subprocess", project="/home/user/myproject")
# Returns: ("/usr/lib/python/rye/.ai/tools/subprocess.py", "system")
```

**Result:** Chain spans multiple spaces with explicit tracking.

---

## Cross-Space Chain Validation

When execution chains span multiple spaces, RYE validates that dependencies are valid.

### Space Compatibility Rules

A tool can depend on tools from **equal or higher precedence spaces** only.

| Child Space | Parent Space | Valid? | Reason |
| ------------ | ------------- | -------- | ------ |
| project | user | ✅ Yes | Project has higher precedence than user |
| project | system | ✅ Yes | Project has highest precedence |
| user | system | ✅ Yes | User has higher precedence than system |
| user | project | ❌ No | User cannot depend on project-specific tools |
| system | project | ❌ No | System immutable, cannot depend on mutable project tools |
| system | user | ❌ No | System immutable, cannot depend on mutable user tools |
| project | project | ✅ Yes | Same space |
| user | user | ✅ Yes | Same space |
| system | system | ✅ Yes | Same space |

### Validation Logic

```python
def validate_space_compatibility(
    child: Dict[str, Any],
    parent: Dict[str, Any],
) -> ValidationResult:
    """
    Validate that tools from different spaces are compatible.
    """
    child_space = child.get("space", "")
    parent_space = parent.get("space", "")

    # Precedence mapping
    precedence = {"project": 3, "user": 2, "system": 1}

    # Lower precedence depending on higher precedence: Invalid
    if precedence.get(child_space, 0) < precedence.get(parent_space, 0):
        return ValidationResult(
            valid=False,
            issues=[
                f"Tool '{child['item_id']}' from {child_space} space cannot "
                f"depend on '{parent['item_id']}' from {parent_space} space. "
                f"Use a higher-precedence space version or pin the dependency."
            ]
        )

    return ValidationResult(valid=True, issues=[])
```

### Valid Chain Example

```python
chain = [
    {"item_id": "git", "space": "project"},
    {"item_id": "python_runtime", "space": "system"},
    {"item_id": "subprocess", "space": "system"},
]

# Validation:
# git (project) → python_runtime (system): ✅ Valid (project > system)
# python_runtime (system) → subprocess (system): ✅ Valid (same space)
```

### Invalid Chain Example

```python
chain = [
    {"item_id": "user_tool", "space": "user"},
    {"item_id": "project_tool", "space": "project"},
]

# Validation:
# user_tool (user) → project_tool (project): ❌ Invalid
# Error: Tool from user space cannot depend on tool from project space
```

---

## Shadowing Behavior

Shadowing = Creating a tool with the same `item_id` in a higher-precedence space.

### Shadowing Rules

| Scenario | Behavior | Example |
| -------- | --------- | -------- |
| Project exists, User creates same ID | Project wins | User creates `git.py`, project has `git.py` → Project wins |
| User exists, System has same ID | User wins | System has `bootstrap.py`, user creates `bootstrap.py` → User wins |
| All three have same ID | Project wins | `git.py` exists in all three → Project wins |

### Why Shadowing is Intentional

Shadowing allows customization and experimentation:

```python
# System provides default tool
# /usr/lib/python/rye/.ai/tools/git.py

# User creates custom version
# ~/.ai/tools/git.py (shadows system)

# Now all executions use user's version
execute("git") → Uses ~/.ai/tools/git.py
```

### Unshadowing

To revert to a lower-precedence version, delete the higher-precedence tool:

```bash
# Remove project version, user version takes precedence
rm /home/user/myproject/.ai/tools/git.py

# Remove user version, system version takes precedence
rm ~/.ai/tools/git.py
```

---

## Complete Validation Example

Here's a complete example showing resolution and validation:

```python
# Chain request
item_id = "custom_scraper"
project_path = "/home/user/myproject"

# Step 1: Resolve tool
tool_path, space = resolve_tool(item_id, project_path)
# Returns: ("/home/user/myproject/.ai/tools/custom_scraper.py", "project")

# Step 2: Load metadata
metadata = parse_metadata(tool_path)
# Returns: {"__tool_type__": "python", "__executor_id__": "python_runtime", ...}

# Step 3: Resolve executor chain
chain = []
current_item_id = item_id
current_space = space

while current_item_id:
    # Resolve current item
    item_path, item_space = resolve_tool(current_item_id, project_path)
    item_metadata = parse_metadata(item_path)

    # Track in chain
    chain.append({
        "item_id": current_item_id,
        "space": item_space,
        "type": item_metadata["__tool_type__"],
    })

    # Check if primitive
    if item_metadata["__executor_id__"] is None:
        break

    # Move to executor
    current_item_id = item_metadata["__executor_id__"]

# Step 4: Validate chain
result = validate_chain(chain)

if not result.valid:
    raise ChainValidationError(result.issues)

# Step 5: Execute
execute_chain(chain)
```

### Valid Chain Result

```python
chain = [
    {"item_id": "custom_scraper", "space": "project"},
    {"item_id": "python_runtime", "space": "system"},
    {"item_id": "subprocess", "space": "system"},
]

validation = validate_chain(chain)
# Result: valid=True, issues=[]
```

### Invalid Chain Result

```python
chain = [
    {"item_id": "system_tool", "space": "system"},
    {"item_id": "project_tool", "space": "project"},
]

validation = validate_chain(chain)
# Result: valid=False,
#         issues=[
#             "Tool 'system_tool' from system space cannot "
#             "depend on 'project_tool' from project space"
#         ]
```

---

## Edge Cases and Gotchas

### 1. Circular Dependencies

```python
# INVALID: Circular chain
tool_a delegates to tool_b
tool_b delegates to tool_a
```

**Detection**: Chain validation tracks visited tools and detects cycles.

### 2. Missing Executors

```python
# Tool references non-existent executor
__executor_id__ = "nonexistent_runtime"
```

**Detection**: Resolution fails with `ToolNotFoundError`.

### 3. Version Mismatches

```python
# Parent requires specific version
parent = {"child_constraints": {"child": {"min_version": "2.0.0"}}}

# Child doesn't meet requirement
child = {"version": "1.0.0"}
```

**Detection**: Version constraint validation in `validate_chain()`.

---

## Related Documentation

- [executor/overview](executor/overview.md) - Tool discovery and routing
- [executor/chain-validator](executor/chain-validator.md) - Detailed validation logic
- [bundle/structure](bundle/structure.md) - Tool organization
- [principles](principles.md) - On-demand loading model
