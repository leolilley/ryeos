# SubprocessPrimitive

## Purpose

Execute shell commands and scripts in isolated environments with timeout protection, environment variable resolution, and comprehensive error handling.

## Key Classes

### SubprocessResult

Result of subprocess execution:

```python
@dataclass
class SubprocessResult:
    success: bool           # Whether exit code was 0
    stdout: str            # Standard output
    stderr: str            # Standard error
    return_code: int       # Process exit code
    duration_ms: int       # Total execution time
```

### SubprocessPrimitive

The executor primitive:

```python
class SubprocessPrimitive:
    async def execute(
        self,
        config: Dict[str, Any],
        params: Dict[str, Any]
    ) -> SubprocessResult:
        """Execute a subprocess command."""
```

## The `params` Parameter

SubprocessPrimitive uses `params` for **runtime parameter templating** in command, args, cwd, and input_data. This mirrors HttpClientPrimitive's URL/body templating.

| `params` Key | Purpose | Example |
|-------------|---------|---------|
| `{any_key}` | Template placeholders in args | `{"input_file": "data.csv"}` |
| `{any_key}` | Template placeholders in command | `{"script": "process.py"}` |
| `{any_key}` | Template placeholders in cwd | `{"project": "/path/to/project"}` |
| `{any_key}` | Template placeholders in input_data | `{"query": "SELECT * FROM users"}` |

```python
# Template args with runtime params
result = await subprocess.execute(
    config={
        "command": "python",
        "args": ["process.py", "--input", "{input_file}", "--output", "{output_file}"]
    },
    params={"input_file": "data.csv", "output_file": "result.json"}
)
# Args become: ["process.py", "--input", "data.csv", "--output", "result.json"]
```

### Two Templating Systems

SubprocessPrimitive supports **two** templating syntaxes that are applied in order:

1. **Environment variables** (`${VAR:-default}`): Resolved first using merged environment
2. **Runtime params** (`{param_name}`): Resolved second using `params` dict

```python
result = await subprocess.execute(
    config={
        "command": "${PYTHON:-python3}",  # Env var with default
        "args": ["script.py", "--file", "{input_file}"]  # Runtime param
    },
    params={"input_file": "data.csv"}
)
# If PYTHON env var is "/usr/bin/python3":
#   command = "/usr/bin/python3"
#   args = ["script.py", "--file", "data.csv"]
```

**Note:** If a `{param}` placeholder is not found in `params`, it is left unchanged (not an error). This allows mixing with other template systems.

## Configuration

### Required

- **`command`** (str): Executable name or path
  - Example: `"python"`, `"bash"`, `"/usr/bin/node"`

### Optional

- **`args`** (list): Command arguments
  - Example: `["script.py", "--verbose", "input.txt"]`

- **`env`** (dict): Environment variables
  - Will be merged with `os.environ` (unless >50 vars)
  - Supports `${VAR:-default}` syntax for defaults
  - Example: `{"DEBUG": "1", "LOG_LEVEL": "info"}`

- **`cwd`** (str): Working directory
  - Default: current working directory
  - Example: `"/home/user/project"`

- **`timeout`** (int): Timeout in seconds
  - Default: 300 seconds
  - Example: `30`

- **`capture_output`** (bool): Capture stdout/stderr
  - Default: `True`
  - If `False`, output goes to parent stdout/stderr

- **`input_data`** (str): Data to send to stdin
  - Default: None
  - Example: `"line1\nline2\n"`

## Configuration Parameter Flow

### Three Distinct Parameter Sources

| Parameter Source | Purpose                                          | Example                                 | Priority |
| ---------------- | ------------------------------------------------ | --------------------------------------- | -------- |
| **config**       | Static tool configuration (from tool definition) | `{"command": "python", "timeout": 300}` | Base     |
| **params**       | Runtime parameters passed by orchestrator        | `{"input_file": "data.csv"}`            | Medium   |
| **env**          | Environment variables (resolved or direct)       | `{"DEBUG": "1", "PATH": "..."}`         | High     |

### How Parameters Flow

```
Tool Definition (in .ai/tools/my_tool.py)
    ↓
    CONFIG = {
        "command": "python",
        "args": ["script.py"],
        "timeout": 300
    }
    ↓
RYE Execution (orchestrator)
    ↓
    params = {"input_file": "data.csv"}  # From user/LLM
    resolved_env = {"DEBUG": "1", ...}  # From ENV_CONFIG
    ↓
    # Templates expanded in CONFIG
    config["command"] = "${PYTHON}"  # If PYTHON in env
    ↓
    Call Lilux primitive:
    SubprocessPrimitive.execute(
        config=expanded_config,  # With templates resolved
        params=params,  # Runtime parameters
    )
    ↓
    # Primitive merges:
    final_config = {**config, **{"env": resolved_env}}
    # Result: config["command"] + env["DEBUG"] + params["input_file"]
```

### Parameter Precedence Rules

For environment variables specifically:

1. **Tool env** (from `config["env"]`) - Highest
2. **Runtime resolved env** (from `ENV_CONFIG`) - Medium
3. **System os.environ** - Lowest
4. **params dict** - Used differently (see below)

### Special Note: `params` vs `env`

**`params` is NOT merged into `config["env"]** - they serve different purposes:

- **`params`**: Runtime execution parameters passed by orchestrator
  - Example: `params={"input_file": "data.csv"}`
  - Used by: Tool's `main()` function (if python tool)

- **`env`**: Environment variables for subprocess
  - Example: `env={"DEBUG": "1", "PYTHON_PATH": "/path/to/python"}`
  - Used by: SubprocessPrimitive to set environment

### Complete Example

```python
# 1. Tool definition
CONFIG = {
    "command": "${PYTHON}",
    "args": ["script.py"],
    "env": {
        "BASE_PATH": "/data"  # Default env var
    }
}

# 2. RYE resolution
ENV_CONFIG = {
    "interpreter": {...},  # Resolves PYTHON="/usr/bin/python3"
    "env": {
        "DEBUG": "1",  # Runtime override
        "OVERRIDE_PATH": "/override"  # Runtime override
    }
}

# 3. Orchestrator calls
params = {
    "input_file": "user_input.csv"  # User parameter
}

# 4. Final configuration after RYE processing
config = {
    "command": "/usr/bin/python3",  # Template expanded
    "args": ["script.py"],
    "env": {
        "BASE_PATH": "/data",  # From tool CONFIG
        "DEBUG": "1",  # From ENV_CONFIG
        "OVERRIDE_PATH": "/override",  # From ENV_CONFIG
        "PYTHON": "/usr/bin/python3"  # From interpreter resolution
    }
}

# 5. Lilux receives
SubprocessPrimitive.execute(
    config=config,  # Complete with all env vars
    params=params  # Runtime parameters
)
```

**Key Point:** `params` is NOT merged into `config`. They remain separate for the primitive to use as needed.

- **`cwd`** (str): Working directory
  - Default: current working directory
  - Example: `"/home/user/project"`

- **`timeout`** (int): Timeout in seconds
  - Default: 300 seconds
  - Example: `30`

- **`capture_output`** (bool): Capture stdout/stderr
  - Default: `True`
  - If `False`, output goes to parent stdout/stderr

- **`input_data`** (str): Data to send to stdin
  - Default: None
  - Example: `"line1\nline2\n"`

## Example Usage

### Simple Command

```python
from lilux.primitives import SubprocessPrimitive

primitive = SubprocessPrimitive()

result = await primitive.execute(
    config={
        "command": "echo",
        "args": ["Hello world"]
    },
    params={}
)

assert result.success == True
assert result.stdout == "Hello world\n"
assert result.return_code == 0
```

### Python Script

```python
result = await primitive.execute(
    config={
        "command": "python",
        "args": ["script.py", "--input", "data.json"],
        "cwd": "/home/user/project",
        "env": {"DEBUG": "1"},
        "timeout": 60
    },
    params={}
)

if result.success:
    print(f"Output: {result.stdout}")
else:
    print(f"Error: {result.stderr}")
    print(f"Exit code: {result.return_code}")
```

### With Environment Variables

```python
result = await primitive.execute(
    config={
        "command": "bash",
        "args": ["-c", "echo $MESSAGE"],
        "env": {"MESSAGE": "Hello from env"}
    },
    params={}
)

assert result.stdout == "Hello from env\n"
```

### With Input Data (stdin)

```python
result = await primitive.execute(
    config={
        "command": "grep",
        "args": ["pattern"],
        "input_data": "line1\npattern\nline3"
    },
    params={}
)

assert "pattern" in result.stdout
```

## Architecture Role

SubprocessPrimitive is part of the **Lilux microkernel execution layer**:

1. **Dumb execution** - Just runs commands, no intelligence
2. **Async-first** - All execution is async (uses asyncio)
3. **Safe isolation** - Subprocess runs in separate process
4. **Comprehensive errors** - Never throws, returns structured results

## Usage

SubprocessPrimitive executes shell commands with automatic environment variable resolution and template expansion.

**Pattern:**

```python
from lilux.primitives.subprocess import SubprocessPrimitive

subprocess_prim = SubprocessPrimitive()

result = await subprocess_prim.execute(
    config={
        "command": "python",
        "args": ["script.py"],
    },
    params={"WORKING_DIR": "/path/to/project"}
)
```

## Environment Variable Resolution

### Pattern: `${VAR:-default}`

SubprocessPrimitive resolves variables in commands and args:

```python
config = {
    "command": "python",
    "args": ["${SCRIPT_PATH:-script.py}"],
    "cwd": "${HOME}/projects"
}

# If SCRIPT_PATH not set, uses "script.py"
# If HOME="C:\Users\alice", uses "C:\Users\alice/projects"
```

### Variable Merging

- If `env` has <50 vars: merged with `os.environ`
- If `env` has ≥50 vars: used directly (assumed already complete from external resolver)

**Why 50?** This is a heuristic to detect when the orchestrator has already resolved a complete environment (via EnvResolver). A fully-resolved environment typically includes the entire `os.environ` plus additional variables, easily exceeding 50 entries. This threshold is not configurable—it's an implementation detail that works well in practice.

## Error Handling

All errors are returned as `SubprocessResult`, never thrown:

### Command Not Found

```python
result = await subprocess.execute(
    config={"command": "nonexistent_command"},
    params={}
)

assert result.success == False
assert "not found" in result.stderr.lower()
assert result.return_code == -1
```

### Timeout

```python
result = await subprocess.execute(
    config={
        "command": "sleep",
        "args": ["30"],
        "timeout": 1
    },
    params={}
)

assert result.success == False
assert "timed out" in result.stderr.lower()
```

### Permission Denied

```python
result = await subprocess.execute(
    config={"command": "/root/private_script"},
    params={}
)

assert result.success == False
assert "Permission denied" in result.stderr
```

## Performance Metrics

`duration_ms` field tracks execution time:

```python
result = await subprocess.execute(config, params)
print(f"Execution took {result.duration_ms}ms")
```

Useful for:

- Performance monitoring
- SLA tracking
- Detecting slow operations

## Testing

SubprocessPrimitive is fully testable:

```python
import pytest
from lilux.primitives import SubprocessPrimitive

@pytest.mark.asyncio
async def test_echo_command():
    prim = SubprocessPrimitive()
    result = await prim.execute(
        config={"command": "echo", "args": ["test"]},
        params={}
    )
    assert result.success
    assert "test" in result.stdout
```

## Limitations and Design

### By Design (Not a Bug)

1. **No shell=True**
   - Direct execution, not shell interpretation
   - Safer and more explicit

2. **No streaming**
   - Output captured after completion
   - For streaming, use `[[lilux/primitives/http-client]]`

3. **No shell pipes**
   - Use `bash -c "cmd1 | cmd2"` for pipes
   - Keeps primitive simple

4. **No interactive input**
   - Can only send stdin at start via `input_data`
   - For interactive tools, use `[[lilux/primitives/http-client]]` with WebSockets

### Security Notes

- Runs subprocess in separate process (OS-level isolation)
- No modification of parent process
- Can be sandboxed further by parent container
- Orchestrator validates command before execution (trust model)

## Runtime Service Integration

### How EnvResolver is Used

EnvResolver is **NOT passed to SubprocessPrimitive directly**. Instead, the orchestrator resolves environment first, then passes the result to the primitive.

**Incorrect Pattern:**

```python
# ❌ Don't do this
primitive = SubprocessPrimitive(env_resolver=env_resolver)
result = await primitive.execute(config, params)
```

**Correct Pattern:**

```python
# ✅ Do this
env_resolver = EnvResolver(project_path="/project")

# Orchestrator resolves environment
resolved_env = env_resolver.resolve(env_config, tool_env)

# Orchestrator passes resolved environment to primitive
config = {
    "command": "python",
    "args": ["script.py"],
    "env": resolved_env  # Fully resolved env dict
}

result = await primitive.execute(config, params)
```

### Why This Approach?

| Approach                           | Pros                         | Cons                             |
| ---------------------------------- | ---------------------------- | -------------------------------- |
| **Primitive receives env dict**    | Simple, stateless, testable  | Orchestrator does more work      |
| **Primitive receives EnvResolver** | Declarative, lazy resolution | Tighter coupling, harder to mock |

**Lilux chooses: Primitive receives env dict** for maximum flexibility and testability.

## Next Steps

- See error handling: `[[lilux/primitives/overview#error-propagation-strategy]]`
- See runtime services: `[[lilux/runtime-services/env-resolver]]`
- See HTTP client: `[[lilux/primitives/http-client]]`
- See executor framework: `[[lilux/primitives/overview]]`
