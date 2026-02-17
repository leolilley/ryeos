# Tool Metadata Reference

Complete specification of metadata fields for tools in Rye OS.

## Overview

Tools are extensible components that define what parameters they accept, what capabilities they require, and what they produce. Tools can be written in Python, YAML, JavaScript, Bash, TOML, or other formats. Each tool declares its type, the executor that runs it, metadata for discovery, and a schema for parameter validation.

Tools are versioned independently (not git-controlled) and distributed via local or online registries.

---

## Required Fields

### `tool_id`

**Type:** string (kebab-case)  
**Required:** Yes

Unique identifier for the tool. Used to reference tool in directives.

```yaml
tool_id: deploy-service
```

### `category`

**Type:** string (non-empty)  
**Required:** Yes

Categorizes the tool. Must match the directory path relative to the `tools/` parent directory.

```yaml
category: deployment/kubernetes
```

**Examples:**

For file at `.ai/tools/deployment/kubernetes/deploy.yaml`:

```yaml
category: deployment/kubernetes
```

For file at `.ai/tools/deployment/deploy-service.js`:

```yaml
category: deployment
```

### `tool_type`

**Type:** enum  
**Required:** Yes  
**Values:** Non empty string

Type classification for the tool.

```yaml
tool_type: script
```

**Examples:**

- `primitive` - Atomic, low-level operation (e.g., `subprocess`, `filesystem`)
- `script` - Executable script in a specific language
- `runtime` - Language runtime or execution environment
- `library` - Reusable library/module without direct execution

### `version`

**Type:** semantic version string (X.Y.Z)  
**Required:** Yes

Version of the tool following semantic versioning.

```yaml
version: "1.0.0"
```

### `description`

**Type:** string  
**Required:** Yes

Clear description of what the tool does.

```yaml
description: "Deploy service to Kubernetes cluster with rolling updates"
```

### `executor_id`

**Type:** string or null  
**Required:** Yes (can be null for primitives)

ID of the executor/runtime that runs this tool. Null for primitive tools.

```yaml
executor_id: python_runtime
```

Common executors:

- `subprocess` - Shell subprocess execution
- `python_runtime` - Python interpreter
- `node_runtime` - Node.js runtime
- `docker` - Docker container execution

### `__tool_description__` (Python/Node.js)

**Type:** string  
**Required:** Yes (for Python and Node.js tools)

Required module-level variable for Python and Node.js tools. This is the official description used for tool discovery and registry. Different from docstring which documents implementation details.

```python
__tool_description__ = "Deploy service to Kubernetes cluster with rolling updates"
```

```javascript
/**
 * @description Deploy service to Kubernetes cluster with rolling updates
 */
```

**Important:** `__tool_description__` is extracted as the `description` field during metadata extraction. The docstring (`__docstring__`) is available separately but not required.

---

## Optional Fields

### `requires`

**Type:** list of strings  
**Purpose:** Capabilities this tool requires

```yaml
requires:
  - rye.execute.spawn.thread
  - rye.execute
  - fs.write
```

**Common capabilities:**

- `rye.execute.spawn.thread` - Ability to spawn new threads
- `rye.execute` - Execute rye operations
- `fs.read`, `fs.write` - File system access
- `shell.execute` - Shell command execution
- `net.http` - HTTP network access

### `parameters`

**Type:** list of parameter specifications  
**Purpose:** Define inputs the tool accepts

```yaml
parameters:
  - name: service_name
    type: string
    required: true
    description: "Name of service to deploy"

  - name: replicas
    type: integer
    required: false
    default: 3
    description: "Number of replicas (1-10)"
    minimum: 1
    maximum: 10

  - name: timeout
    type: integer
    required: false
    default: 300
    description: "Timeout in seconds"
```

**Parameter properties:**

- `name` (required) - Parameter identifier
- `type` (required) - Type: `string`, `integer`, `float`, `boolean`, `object`, `array`
- `required` - Whether parameter is mandatory (default: false)
- `default` - Default value if not provided
- `description` - Human-readable description
- `minimum`, `maximum` - Numeric constraints
- `minLength`, `maxLength` - String length constraints
- `pattern` - Regex pattern for string validation
- `enum` - List of allowed values

### `outputs`

**Type:** object or schema  
**Purpose:** Document what the tool returns

```yaml
outputs:
  type: object
  properties:
    deployment_id:
      type: string
      description: "ID of created deployment"
    status:
      type: string
      enum: ["success", "failed", "pending"]
    details:
      type: object
      description: "Additional deployment details"
  required:
    - deployment_id
    - status
```

### `actions`

**Type:** list of action definitions  
**Purpose:** Define callable sub-actions within the tool

```yaml
actions:
  deploy:
    description: "Deploy service"
    parameters:
      - name: service_name
        type: string
        required: true

  rollback:
    description: "Rollback to previous version"
    parameters:
      - name: deployment_id
        type: string
        required: true
```

### `tags`

**Type:** list of strings  
**Purpose:** Searchable tags for discoverability

```yaml
tags:
  - kubernetes
  - deployment
  - rolling-update
  - cloud
```

### `config`

**Type:** object  
**Purpose:** Tool-specific configuration

For subprocess tools:

```yaml
config:
  command: python
  args:
    - deploy.py
  env:
    KUBECONFIG: /etc/kubernetes/config
    LOG_LEVEL: INFO
```

For Docker tools:

```yaml
config:
  image: python:3.11
  command: python
  entrypoint: /app/deploy.py
  env:
    - KUBECONFIG
    - AWS_CREDENTIALS
```

### `input_schema`

**Type:** JSON Schema object  
**Purpose:** Complete parameter validation schema

```yaml
input_schema:
  type: object
  properties:
    service_name:
      type: string
      minLength: 3
      pattern: "^[a-z][a-z0-9-]*$"
    replicas:
      type: integer
      minimum: 1
      maximum: 10
  required:
    - service_name
  additionalProperties: false
```

### `output_schema`

**Type:** JSON Schema object  
**Purpose:** Complete output validation schema

```yaml
output_schema:
  type: object
  properties:
    deployment_id:
      type: string
      pattern: "^[a-f0-9-]+$"
    status:
      type: string
      enum: ["success", "failed", "pending"]
    duration_seconds:
      type: number
      minimum: 0
  required:
    - deployment_id
    - status
```

### `timeout`

**Type:** integer (seconds)  
**Purpose:** Default execution timeout

```yaml
timeout: 300
```

### `retry_policy`

**Type:** object  
**Purpose:** Define retry behavior on failure

```yaml
retry_policy:
  max_attempts: 3
  backoff_type: exponential
  initial_delay: 1
  max_delay: 60
  backoff_multiplier: 2
```

### `cost`

**Type:** object  
**Purpose:** Cost estimation for tool execution

```yaml
cost:
  per_invocation: 0.01
  per_minute: 0.05
  estimated_duration: 60
  currency: USD
```

### `documentation`

**Type:** object  
**Purpose:** Links to documentation and examples

```yaml
documentation:
  url: "https://docs.example.com/tools/deploy"
  examples:
    - "Deploy to production"
    - "Scale service replicas"
```

### `metadata`

**Type:** object  
**Purpose:** Additional metadata fields

```yaml
metadata:
  author: platform-team
  maintainer: ops@example.com
  deprecated: false
  experimental: false
  stability: stable
```

---

## Complete Examples

### Python Tool

```python
# .ai/tools/utility/hello_world.py
__version__ = "1.0.1"
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "utility"
__tool_description__ = "Say hello to someone with a personalized greeting message"

def main(name: str = "World") -> str:
    """
    Say hello to someone.
    
    Args:
        name: The name to greet (default: World)
    
    Returns:
        A greeting message
    """
    message = f"Hello, {name}!"
    return message
```

### Node.js Tool

```javascript
// .ai/tools/utility/hello_node.js
/**
 * Say hello to someone with a personalized greeting message
 * 
 * @version 1.0.0
 * @tool_type javascript
 * @executor_id node_runtime
 * @category utility
 * @description Say hello to someone with a personalized greeting message
 */

const yargs = require('yargs/yargs');
const { hideBin } = require('yargs/helpers');

const argv = yargs(hideBin(process.argv))
  .option('name', {
    alias: 'n',
    type: 'string',
    description: 'Name to greet',
    default: 'World'
  })
  .help()
  .argv;

function main() {
  console.log(`Hello, ${argv.name}!`);
}

if (require.main === module) {
  main();
}

module.exports = { main };
```

### YAML Tool (HTTP)

```yaml
# .ai/tools/mcp/mcp_http.yaml
tool_id: mcp_http
tool_type: http
version: "1.0.0"
description: "Execute JSON-RPC call over HTTP MCP connection"
executor_id: http_client
category: mcp

requires:
  - net.call

config:
  method: POST
  url: "{url}"
  headers:
    Content-Type: application/json
  body:
    jsonrpc: "2.0"
    id: "{request_id}"
    method: "{rpc_method}"
    params: "{rpc_params}"

parameters:
  - name: url
    type: string
    required: true
    description: "MCP server URL"
  - name: rpc_method
    type: string
    required: true
    description: "JSON-RPC method (e.g., tools/call)"
  - name: rpc_params
    type: object
    required: true
    description: "JSON-RPC params"
```

### Python Runtime Tool (Class-based)

```python
# .ai/tools/sinks/file_sink.py
__tool_type__ = "runtime"
__version__ = "1.0.0"
__executor_id__ = "python"
__category__ = "sinks"
__tool_description__ = "Append streaming events to file in JSONL or plain text format"

import json
from pathlib import Path
from typing import Optional


class FileSink:
    """Append streaming events to file."""

    def __init__(self, path: str, format: str = "jsonl", flush_every: int = 10):
        self.path = Path(path)
        self.format = format
        self.flush_every = flush_every
        self.event_count = 0
        self.file_handle: Optional[str] = None

    async def write(self, event: str) -> None:
        """Write event to file."""
        if not self.file_handle:
            self.file_handle = open(self.path, "a", encoding="utf-8")

        if self.format == "jsonl":
            try:
                data = json.loads(event)
                self.file_handle.write(json.dumps(data) + "\n")
            except json.JSONDecodeError:
                self.file_handle.write(event + "\n")
        else:
            self.file_handle.write(event + "\n")

        self.event_count += 1
```

---

## Validation Rules

1. **Required fields:** `tool_id`, `category`, `tool_type`, `version`, `description`, `executor_id`
   - **Python/Node.js tools:** Must also define `__tool_description__` module variable (extracted as `description`)
   - **YAML/JSON tools:** `description` field in metadata
2. **`tool_id`** must be kebab-case alphanumeric
3. **`category`** must match the file path relative to `tools/` directory
4. **`version`** must be semantic version (X.Y.Z)
5. **`executor_id`** can be null only for `primitive` tool_type
6. **Parameter `type`** must be valid JSON Schema type
7. **`requires`** list capabilities using dotted notation (e.g., `fs.write`)
8. **`pattern`** fields must be valid regex
9. **`enum`** must have at least one value

---

## Best Practices

### Naming

- Use kebab-case for `tool_id`: `deploy-service`, not `DeployService`
- Be specific and descriptive: `deploy-kubernetes`, not `deploy`
- Include action/domain if unclear: `resize-pool`, not `resize`

### Parameters

- Use snake_case for parameter names
- Provide `description` for every parameter
- Always specify `required` status (default: false)
- Add constraints (`minimum`, `maximum`, `pattern`) for clarity
- Use `enum` for constrained values

### Documentation

- Describe what, not how: "Deploy service to cluster" not "Run deploy_k8s.py"
- Include examples in tool descriptions
- Document all parameters clearly
- Specify output format exactly

### Capabilities

- Declare all required capabilities in `requires`
- Use dotted notation: `fs.read`, `shell.execute`
- Fail fast if capability missing (don't silently degrade)

### Error Handling

- Define expected error conditions in documentation
- Use retry_policy for transient failures
- Document timeout behavior
- Specify what constitutes success/failure

---

## Tool Types

### Primitive

No executor, atomic operation. Examples:

- `subprocess` - Shell command execution
- `http_client` - HTTP requests
- `filesystem` - File operations

### Script

Executable script in specific language:

```yaml
tool_type: script
executor_id: python_runtime
```

### Runtime

Language runtime/environment:

```yaml
tool_type: runtime
executor_id: null
```

### Library

Reusable code without direct execution:

```yaml
tool_type: library
executor_id: null
```

---

## References

- [JSON Schema Specification](https://json-schema.org/)
- [Kiwi MCP Tool Registry](https://github.com/example/kiwi-mcp/docs/TOOL_VALIDATION_DESIGN.md)
