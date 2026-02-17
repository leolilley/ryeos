# EnvResolver Service

## Purpose

Resolve environment variables from multiple sources using ENV_CONFIG rules. Applies generic resolver types (venv_python, node_modules, system_binary, version_manager) to find interpreters and construct complete execution environments.

## Architecture

EnvResolver is a **pure resolver** (no side effects):

```
EnvResolver.resolve(env_config, tool_env)
    ↓
1. Start with os.environ
2. Load .env files (optional)
3. Apply ENV_CONFIG rules (interpreter resolution)
4. Apply static env vars
5. Apply tool-level overrides
    ↓
Return complete environment dict
```

## Key Classes

### EnvResolver

The environment resolver service:

```python
class EnvResolver:
    def __init__(self, project_path: Optional[Path] = None):
        """
        Initialize with project context.
        
        Args:
            project_path: Root directory for relative path resolution.
                         If None, uses current working directory.
                         Used for:
                         - Finding .venv directories
                         - Loading .env files
                         - Resolving relative paths in config
        """
    
    def resolve(
        self,
        env_config: Optional[Dict[str, Any]] = None,
        tool_env: Optional[Dict[str, str]] = None,
        include_dotenv: bool = True
    ) -> Dict[str, str]:
        """Resolve environment from all sources."""
```

**Note:** If `project_path` is `None` and you use `venv_python` resolver with a relative `venv_path` like `.venv`, it will search relative to `os.getcwd()`. For predictable behavior, always provide an explicit `project_path`.

## Resolver Types

EnvResolver supports 4 resolver types:

### 1. venv_python

Find Python in virtual environment:

```python
{
    "type": "venv_python",
    "var": "PYTHON_PATH",
    "venv_path": ".venv",           # Where to look
    "fallback": "/usr/bin/python3"  # If not found
}
```

**Searches:**
- `.venv/bin/python` (Unix)
- `.venv\Scripts\python.exe` (Windows)
- Other venv locations

### 2. node_modules

Find Node.js in node_modules:

```python
{
    "type": "node_modules",
    "var": "NODE_PATH",
    "search_paths": ["node_modules/.bin"],
    "fallback": "/usr/bin/node"
}
```

**Searches:**
- `node_modules/.bin/node`
- Standard npm locations

### 3. system_binary

Find any binary in system PATH:

```python
{
    "type": "system_binary",
    "var": "RUBY_PATH",
    "binary": "ruby",
    "fallback": "/usr/bin/ruby"
}
```

**Searches:**
- `which ruby` (Unix)
- `where ruby` (Windows)
- System PATH

### 4. version_manager

Find interpreter via version manager:

```python
{
    "type": "version_manager",
    "var": "PYTHON_PATH",
    "manager": "pyenv",        # pyenv, nvm, rbenv, asdf
    "version": "3.9.0",        # Specific version
    "fallback": "/usr/bin/python3"
}
```

**Supports:**
- `pyenv` - Python version manager
- `nvm` - Node.js version manager
- `rbenv` - Ruby version manager
- `asdf` - Multi-language version manager

## Usage Pattern

### Basic Environment Resolution

```python
from lilux.runtime import EnvResolver

resolver = EnvResolver()

# Resolve with system environment + .env files
env = resolver.resolve()

# Use resolved environment
result = await subprocess.execute(
    config={
        "command": "python",
        "env": env
    },
    params={}
)
```

### With Interpreter Resolution

```python
# Configure Python from virtual environment
env_config = {
    "interpreter": {
        "type": "venv_python",
        "var": "PYTHON_PATH",
        "venv_path": ".venv",
        "fallback": "/usr/bin/python3"
    }
}

env = resolver.resolve(env_config=env_config)

# env["PYTHON_PATH"] = "/path/to/.venv/bin/python"
```

### With Static Environment Variables

```python
env_config = {
    "env": {
        "DEBUG": "1",
        "LOG_LEVEL": "debug",
        "DATABASE_URL": "postgresql://localhost/mydb"
    }
}

env = resolver.resolve(env_config=env_config)

# env["DEBUG"] = "1"
# env["LOG_LEVEL"] = "debug"
# env["DATABASE_URL"] = "postgresql://localhost/mydb"
```

### With Variable Expansion

```python
# Use ${VAR} to reference other variables
env_config = {
    "env": {
        "PROJECT_HOME": "/home/user/project",
        "VENV_PATH": "${PROJECT_HOME}/.venv",
        "PYTHON": "${VENV_PATH}/bin/python"
    }
}

env = resolver.resolve(env_config=env_config)

# Variables expanded recursively
# env["PYTHON"] = "/home/user/project/.venv/bin/python"
```

### With Tool-Level Overrides

```python
env_config = {
    "env": {
        "DEBUG": "0",
        "LOG_LEVEL": "info"
    }
}

tool_env = {
    "DEBUG": "1",  # Override
    "CUSTOM": "value"
}

env = resolver.resolve(env_config=env_config, tool_env=tool_env)

# Tool overrides take precedence
# env["DEBUG"] = "1"  (overridden)
# env["LOG_LEVEL"] = "info"
# env["CUSTOM"] = "value"
```

## .env File Support

EnvResolver can load .env files:

```bash
# .env file
DATABASE_URL=postgresql://localhost/mydb
API_KEY=secret123
DEBUG=1
```

```python
# Load .env automatically
env = resolver.resolve(include_dotenv=True)

# env["DATABASE_URL"] = "postgresql://localhost/mydb"
# env["API_KEY"] = "secret123"
# env["DEBUG"] = "1"
```

## Architecture Role

EnvResolver is part of the **runtime services layer**:

1. **Environment assembly** - Combine sources
2. **Interpreter resolution** - Find interpreters
3. **Variable expansion** - Expand templates
4. **Pure resolution** - No side effects (no venv creation)

## Usage

Orchestrators use EnvResolver when:
- Executing subprocess tools
- Need to find Python/Node/Ruby interpreters
- Apply ENV_CONFIG from runtimes

**Pattern:**
```python
# Orchestrator retrieves runtime
runtime = get_runtime(tool.runtime)
env_config = runtime.env_config

env = env_resolver.resolve(
    env_config=env_config,
    tool_env=tool.config.get("env"),
    include_dotenv=True
)

# Execute subprocess with resolved environment
result = await subprocess.execute(
    config={
        "command": "python",
        "env": env
    },
    params={}
)
```

## Variable Expansion

EnvResolver expands `${VAR}` and `${VAR:-default}` syntax:

```python
# Basic expansion
env_config = {
    "env": {
        "HOME": "/home/user",
        "PROJECT": "${HOME}/projects"
    }
}
# env["PROJECT"] = "/home/user/projects"

# With default
env_config = {
    "env": {
        "DEBUG": "${DEBUG_MODE:-false}"  # Use DEBUG_MODE if set, else "false"
    }
}

# If DEBUG_MODE not set:
# env["DEBUG"] = "false"
```

## Complete Example: Python Runtime

### Runtime Configuration

The orchestrator provides runtime configuration to EnvResolver:

```python
env_config = {
    "interpreter": {
        "type": "venv_python",
        "var": "PYTHON",
        "venv_path": ".venv",
        "fallback": "/usr/bin/python3"
    },
    "env": {
        "PYTHONUNBUFFERED": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
        "PIP_REQUIRE_VIRTUALENV": "true"
    }
}
```

### Tool Configuration

```python
tool_config = {
    "tool_id": "run_script",
    "executor": "subprocess",
    "config": {
        "command": "${PYTHON}",
        "args": ["script.py"],
        "env": {"DEBUG": "1"}
    }
}
```

### Execution Flow

```python
# 1. Orchestrator loads runtime config and passes to EnvResolver
resolver = EnvResolver(project_path="/home/user/project")
env = resolver.resolve(
    env_config=env_config,
    tool_env={"DEBUG": "1"}
)

# Result:
# env["PYTHON"] = "/home/user/project/.venv/bin/python"
# env["PYTHONUNBUFFERED"] = "1"
# env["PYTHONDONTWRITEBYTECODE"] = "1"
# env["PIP_REQUIRE_VIRTUALENV"] = "true"
# env["DEBUG"] = "1"

# 2. Execute subprocess with resolved environment
result = await subprocess.execute(
    config={
        "command": "/home/user/project/.venv/bin/python",
        "args": ["script.py"],
        "env": env
    },
    params={}
)
```

## Testing

```python
import pytest
from lilux.runtime import EnvResolver
from pathlib import Path

def test_basic_resolution():
    resolver = EnvResolver()
    env = resolver.resolve()
    
    # Should include system environment
    assert "PATH" in env
    assert "HOME" in env

def test_with_static_env():
    resolver = EnvResolver()
    env_config = {
        "env": {
            "DEBUG": "1",
            "CUSTOM": "value"
        }
    }
    
    env = resolver.resolve(env_config=env_config)
    
    assert env["DEBUG"] == "1"
    assert env["CUSTOM"] == "value"

def test_with_tool_overrides():
    resolver = EnvResolver()
    env_config = {"env": {"DEBUG": "0"}}
    tool_env = {"DEBUG": "1"}
    
    env = resolver.resolve(env_config=env_config, tool_env=tool_env)
    
    assert env["DEBUG"] == "1"  # Tool override wins

def test_variable_expansion():
    resolver = EnvResolver()
    env_config = {
        "env": {
            "HOME": "/home/user",
            "PROJECT": "${HOME}/projects"
        }
    }
    
    env = resolver.resolve(env_config=env_config, include_dotenv=False)
    
    # Would expand to "/home/user/projects" if HOME not in os.environ
```

## Error Handling

### Missing Interpreter

EnvResolver never raises exceptions for missing interpreters. Instead:

**If venv not found AND fallback provided:**
```python
env_config = {
    "interpreter": {
        "type": "venv_python",
        "var": "PYTHON",
        "venv_path": ".venv",
        "fallback": "/usr/bin/python3"
    }
}
env = resolver.resolve(env_config=env_config)
# env["PYTHON"] = "/usr/bin/python3" (uses fallback, even if .venv missing)
```

**If venv not found AND no fallback:**
```python
env_config = {
    "interpreter": {
        "type": "venv_python",
        "var": "PYTHON",
        "venv_path": ".nonexistent"
        # No fallback
    }
}
env = resolver.resolve(env_config=env_config)
# env["PYTHON"] is not set (variable missing from dict)
```

### Design Rationale

EnvResolver is a pure resolver (no exceptions, no side effects):
- Always returns a valid environment dict
- Trusts configuration: if fallback provided, use it (even if fallback path doesn't exist)
- Missing variables are silently omitted from result
- Orchestrator/tool config is responsible for providing valid fallbacks

## Limitations and Design

### By Design (Not a Bug)

1. **Pure resolver only**
   - Doesn't create virtual environments
   - Doesn't install packages
   - Only finds existing interpreters

2. **No dynamic PATH modification**
   - Resolves to absolute paths
   - Doesn't add to PATH
   - Each tool gets complete environment

3. **Linear expansion only**
   - Expands ${VAR} once
   - No complex templates
   - Supports default syntax only

4. **OS-specific**
   - Searches are platform-dependent
   - Windows: `.venv\Scripts\python.exe`
   - Unix: `.venv/bin/python`

## Next Steps

- See AuthStore: `[[lilux/runtime-services/auth-store]]`
- See Lockfile I/O: `[[lilux/primitives/lockfile]]`
- See primitives: `[[lilux/primitives/overview]]`
