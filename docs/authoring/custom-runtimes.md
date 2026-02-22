```yaml
id: custom-runtimes
title: "Custom Runtimes"
description: "How to add support for new languages by creating runtime YAML configurations"
category: authoring
tags: [runtimes, custom, authoring, new-language, yaml, configuration]
version: "1.0.0"
```

# Custom Runtimes

A custom runtime lets you support new languages and execution environments by creating a single YAML file. No code changes to Rye needed — just describe how to invoke a tool, and the system will execute it.

## What a Runtime YAML File Looks Like

A runtime is a YAML file that describes:
1. **Interpreter resolution** — how to find the language's executable
2. **Anchoring** — how to set up module/library search paths
3. **Command template** — arguments and environment variables to pass the tool
4. **Verification** — optional dependency checking

### Minimal Example: Go Runtime

```yaml
# .ai/tools/rye/core/runtimes/go_runtime.yaml
version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes
description: "Go runtime - executes Go binaries"

env_config:
  env:
    PATH: "${PATH}"

config:
  command: "/usr/bin/go"
  args:
    - "run"
    - "{tool_path}"
    - "--"
    - "{params_json}"
    - "{project_path}"
  timeout: 300

config_schema:
  type: object
  properties:
    binary:
      type: string
      description: Go binary or .go file to run
```

## How to Configure Interpreter Resolution

Interpreter resolution finds the language's executable. There are 4 built-in types:

### Type 1: Virtual Environment (`venv_python`)

For Python environments:

```yaml
env_config:
  interpreter:
    type: venv_python
    venv_path: .venv              # Relative to project root
    var: RYE_PYTHON               # Environment variable to set
    fallback: python3             # Fallback if .venv not found
```

**Used by:** `python_function_runtime`, `python_script_runtime`, `state_graph_runtime`

### Type 2: Node Modules (`node_modules`)

For Node.js environments with node_modules:

```yaml
env_config:
  interpreter:
    type: node_modules
    search_paths: [node_modules/.bin]   # Relative to anchor
    var: RYE_NODE                       # Environment variable
    fallback: node                      # Fallback
```

**Used by:** `node_runtime`

### Type 3: Static Environment (`env`)

For simple pass-through or static values:

```yaml
env_config:
  env:
    PATH: "${PATH}"                     # Expand from current environment
    RUBY_VERSION: "3.2.0"               # Static value
    RUST_BACKTRACE: "1"
```

**Used by:** `bash_runtime`, simple tools

### Type 4: Custom Script (extend the system)

For advanced needs, write a custom interpreter resolver. This is beyond this guide — see [Architecture](../internals/architecture.md#layer-2-rye-mcp-server) for `EnvResolver` details.

---

## How to Set Up Anchoring for Module Resolution

Anchoring establishes a project root where libraries live. This enables tools to import/require modules without absolute paths.

### Anchor Configuration

```yaml
anchor:
  enabled: true                           # Enable anchoring
  mode: auto                              # 'auto' (search up) or 'always' (tool dir)
  markers_any: [package.json, Gemfile]   # Stop at first marker found
  root: tool_dir                          # 'tool_dir' is standard
  lib: lib/ruby                           # Relative path for runtime libs
  cwd: "{anchor_path}"                    # Optional: change working dir
  env_paths:
    RUBYLIB:
      prepend: ["{anchor_path}", "{runtime_lib}"]
```

### Mode: `auto` (Recommended)

Walks up directories looking for marker files:

```yaml
anchor:
  enabled: true
  mode: auto
  markers_any: [Gemfile, Gemfile.lock]
  root: tool_dir
  lib: lib/ruby
```

**Process:**
1. Start from tool directory (e.g., `.ai/tools/my/ruby_tool.rb`)
2. Walk up parent directories
3. Stop at first directory containing `Gemfile` or `Gemfile.lock`
4. That's the anchor root
5. If no marker found, use tool directory

**Result:** Tools can `require` from anchor root without absolute paths.

### Mode: `always` (For Strict Isolation)

Tool directory is always the anchor root:

```yaml
anchor:
  enabled: true
  mode: always
  root: tool_dir
  lib: lib/state_graph
```

**Used by:** `state_graph_runtime` (strict graph isolation)

### Anchor Path Variables

Once anchored, reference these in `config.args` and `env_paths`:

| Variable | Expands To | Example |
|----------|-----------|---------|
| `{anchor_path}` | Root of anchored project | `/project/.ai/tools/my` |
| `{runtime_lib}` | Anchor lib + runtime lib | `/project/.ai/tools/my/lib/ruby` |

### Working Directory Control

Change working directory during execution:

```yaml
anchor:
  enabled: true
  cwd: "{anchor_path}"        # Execute from anchor root
```

---

## Step-by-Step Example: Ruby Runtime

Let's create a complete Ruby runtime for educational purposes.

### Step 1: Create the Runtime YAML

Create `.ai/tools/rye/core/runtimes/ruby_runtime.yaml`:

```yaml
version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes
description: "Ruby runtime - executes Ruby scripts with bundler support"

env_config:
  # Ruby interpreter resolution
  interpreter:
    type: venv_python              # Fallback to system ruby for now
    var: RYE_RUBY
    fallback: ruby
  
  # Environment setup
  env:
    BUNDLE_GEMFILE: "{anchor_path}/Gemfile"
    RUBYOPT: "-w"                  # Enable warnings

anchor:
  enabled: true
  mode: auto                        # Walk up looking for Gemfile
  markers_any: [Gemfile, Gemfile.lock]
  root: tool_dir
  lib: lib/ruby
  cwd: "{anchor_path}"              # Execute from anchor root
  env_paths:
    RUBYLIB:
      prepend: ["{anchor_path}", "{anchor_path}/lib", "{runtime_lib}"]

# Optional: verify dependencies
verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [.rb, .yaml, .yml, .json, .lock]
  exclude_dirs: [.bundle, .git, node_modules]

config:
  command: "${RYE_RUBY}"
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 300

config_schema:
  type: object
  properties:
    script:
      type: string
      description: Ruby script path
    args:
      type: array
      items:
        type: string
      description: Script arguments
```

### Step 2: Create a Ruby Tool

Create `.ai/tools/my/ruby_example.rb`:

```ruby
#!/usr/bin/env ruby
# Ruby tool example - receives JSON params via CLI

require "json"
require "optparse"

# Parse CLI arguments
options = {}
OptionParser.new do |opts|
  opts.on("--params JSON", String) { |v| options[:params] = v }
  opts.on("--project-path PATH", String) { |v| options[:project_path] = v }
end.parse!

# Load and parse params
params = options[:params] ? JSON.parse(options[:params]) : {}
project_path = options[:project_path]

begin
  # Execute tool logic
  name = params["name"] || "World"
  greeting = "Hello, #{name}!"
  
  # Return result as JSON
  result = {
    success: true,
    greeting: greeting,
    timestamp: Time.now.iso8601
  }
  
  puts JSON.generate(result)
rescue => e
  puts JSON.generate({
    success: false,
    error: e.message
  })
  exit 1
end
```

### Step 3: Create Tool Metadata

Add metadata to the Ruby tool. Since Ruby doesn't have standard metadata extraction like Python's dunder vars, store metadata in a YAML file alongside:

Create `.ai/tools/my/ruby_example.yaml`:

```yaml
tool_id: my/ruby_example
tool_type: ruby
version: "1.0.0"
executor_id: rye/core/runtimes/ruby_runtime
category: my/examples
description: "Ruby greeting example"
```

### Step 4: Sign and Execute

```bash
# Sign the runtime (one-time)
rye_sign rye/core/runtimes/ruby_runtime

# Sign the tool
rye_sign my/ruby_example

# Execute
rye_execute my/ruby_example --name Alice
```

Expected output:
```json
{
  "success": true,
  "greeting": "Hello, Alice!",
  "timestamp": "2026-02-23T10:45:30Z"
}
```

---

## How Tools Reference the Custom Runtime

A tool references a custom runtime via `__executor_id__`:

### Python Tool Using Custom Runtime

```python
# .ai/tools/my/custom_tool.py
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/ruby_runtime"  # Point to custom runtime
__category__ = "my/tools"
__tool_description__ = "Tool using Ruby runtime"

def execute(params: dict, project_path: str) -> dict:
    return {"success": True, "message": "Hello from Ruby runtime"}
```

### YAML Tool Using Custom Runtime

```yaml
# .ai/tools/my/custom_tool.yaml
tool_id: my/custom_tool
tool_type: yaml
version: "1.0.0"
executor_id: rye/core/runtimes/ruby_runtime
category: my/tools
description: "YAML tool using Ruby runtime"
```

The system resolves the executor ID (`rye/core/runtimes/ruby_runtime`) and uses the runtime's configuration to build the command.

---

## Advanced Patterns

### Chaining Runtimes (Meta-Runtime)

A runtime can itself use another runtime as its executor:

```yaml
# .ai/tools/rye/core/runtimes/jupyter_runtime.yaml
version: "1.0.0"
executor_id: rye/core/runtimes/python_script_runtime  # Chain!
category: rye/core/runtimes
description: "Jupyter notebook executor"

config:
  command: "jupyter"
  args:
    - "nbconvert"
    - "--to=notebook"
    - "--execute"
    - "{tool_path}"
  timeout: 600
```

This creates a "Jupyter runtime" that delegates to Python script runtime for its execution.

### Environment Variable Expansion

First-stage expansion (before execution):

```yaml
env_config:
  interpreter:
    type: venv_python
    var: MY_PYTHON
    fallback: python3

config:
  command: "${MY_PYTHON}"      # Expanded: /path/to/.venv/bin/python3
  args:
    - "{tool_path}"            # Expanded later: /path/to/tool.py
    - "{params_json}"          # Expanded later: {"foo":"bar"}
```

Two-stage process:
1. `${MY_PYTHON}` → resolved interpreter path
2. `{tool_path}`, `{params_json}` → tool invocation parameters

### Conditional Execution (Per-Environment)

Use env variables for conditional behavior:

```yaml
env_config:
  env:
    DEBUG_MODE: "${DEBUG:-0}"
    BUILD_FLAVOR: "${BUILD_FLAVOR:-release}"

config:
  command: "${COMPILER}"
  args:
    - "--debug=${DEBUG_MODE}"
    - "--flavor=${BUILD_FLAVOR}"
    - "{tool_path}"
```

---

## Testing Custom Runtimes

### 1. Create a Test Tool

```python
# .ai/tools/test/runtime_test.py
__executor_id__ = "rye/core/runtimes/ruby_runtime"
__tool_type__ = "python"
__category__ = "test/runtime"

def execute(params: dict, project_path: str) -> dict:
    return {"success": True, "test": "passed"}
```

### 2. Execute and Verify

```bash
rye_execute test/runtime_test
```

Check:
- Exit code is 0
- Output is valid JSON with `success: true`
- No interpreter errors

### 3. Debug with Logging

Add debug output to the runtime args:

```yaml
config:
  command: "${RYE_RUBY}"
  args:
    - "-v"                      # Verbose Ruby
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
```

---

## Common Pitfalls

| Pitfall | Solution |
|---------|----------|
| Tool can't find libraries | Enable anchoring and set `PYTHONPATH`/`NODE_PATH`/`RUBYLIB` in `env_paths` |
| Interpreter not found | Check `fallback` path exists, or set in `env` with static path |
| Working directory wrong | Use `anchor.cwd: "{anchor_path}"` to set working directory |
| Parameters not parsed | Tool must read `--params` and `--project-path` CLI args, or use a wrapper |
| Timeout errors | Increase `config.timeout` in runtime YAML |
| Output not JSON | Tool must return `print(json.dumps(...))` or equivalent |

---

## See Also

- [Runtimes Reference](../internals/runtimes.md) — Complete details on all 7 standard runtimes
- [Authoring Tools](tools.md) — Write tools that use runtimes
- [Executor Chain](../internals/executor-chain.md) — How tools resolve to runtimes to primitives
- [Architecture](../internals/architecture.md) — System layers and data flow
