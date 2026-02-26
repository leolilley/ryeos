<!-- rye:signed:2026-02-26T05:52:24Z:b632de2da88d3826a466fe8dabdfd265bb774e9025ca63b49a6d0cf1005e1e8e:-EkFtSFbNS2e39wC_oVdfnm8QyVuEFa7m4EAbmE-qb6zHtWJqALx3wCieA1HogF5RvtWZbGaMsL6pMJKMgyNBQ==:4b987fd4e40303ac -->

```yaml
name: runtime-authoring
title: "Custom Runtime Authoring Guide"
entry_type: pattern
category: rye/core/runtimes
version: "1.0.0"
author: rye-os
created_at: 2026-02-23T00:00:00Z
tags:
  - runtime
  - authoring
  - custom
  - new-language
  - yaml
  - configuration
  - interpreter-resolution
  - template-variables
  - tool-execution
  - how-to-create-runtime
references:
  - standard-runtimes
  - executor-chain
  - templating-systems
  - "docs/authoring/custom-runtimes.md"
extends:
  - standard-runtimes
```

# Custom Runtime Authoring Guide

How to create a new runtime YAML to add support for a language or protocol not covered by the 7 standard runtimes.

## When to Create a Custom Runtime

**Do create a custom runtime if:**
- You need to support a new language (Ruby, Go, Rust, Kotlin, etc.)
- You're wrapping an unconventional execution model
- You need custom interpreter resolution logic
- You're integrating a domain-specific language

**Don't create a custom runtime if:**
- You can extend an existing one (e.g., Python for a Python-like DSL)
- The execution model fits an existing primitive

## Runtime YAML Structure

```yaml
# Metadata
version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess   # Points to primitive (or custom)
category: category/path

# Configuration
env_config:
  interpreter:
    type: ...
    ...
  env:
    ...

anchor:
  enabled: ...
  ...

verify_deps:
  enabled: ...
  ...

config:
  command: ...
  args: [...]
  timeout: ...

config_schema:
  type: object
  properties: { ... }
```

## Step 1: Choose the Primitive

Every runtime points to a **primitive** — the underlying executor:

| Primitive | For | Examples |
|-----------|-----|----------|
| `rye/core/primitives/subprocess` | Language CLIs, scripts | Python, Node, Ruby, Go, Bash |
| `rye/core/primitives/http_client` | HTTP APIs, REST | Web services, remote tools |
| Custom primitive | Specialized execution | Domain-specific engines |

Most custom runtimes use `subprocess`.

## Step 2: Configure Interpreter Resolution

Where does your language binary live? Pick a resolution type:

### `local_binary`

Searches for a binary in configured local directories (virtual environments, node_modules, etc.):

```yaml
# Python in a virtual environment
env_config:
  interpreter:
    type: local_binary
    binary: python
    candidates: [python3]
    search_paths: [".venv/bin", ".venv/Scripts"]
    var: RYE_PYTHON
    fallback: python3
```

```yaml
# Node/tsx in node_modules
env_config:
  interpreter:
    type: local_binary
    binary: tsx
    search_paths: ["node_modules/.bin"]
    search_roots: ["{anchor_path}"]
    var: RYE_NODE
    fallback: node
```

**When:** Language binaries installed locally — Python venvs, npm packages, or any project-local toolchain.

**Config fields:**
- `binary` — name of the executable to find (e.g., `python`, `tsx`)
- `candidates` — alternative binary names to try (e.g., `[python3]`)
- `search_paths` — relative directories to search within (e.g., `[".venv/bin"]`)
- `search_roots` — base directories to search from (e.g., `["{anchor_path}"]`); defaults to project root
- `var` — env var to store resolved path
- `fallback` — fallback binary name or path if local search fails

### `system_binary`

Finds any binary via `which`/`where`:

```yaml
env_config:
  interpreter:
    type: system_binary
    binary: ruby
    var: RYE_RUBY
    fallback: /usr/bin/ruby
```

**When:** System-installed languages (Ruby, Go, Rust, etc.).

**Config fields:**
- `binary` — name of the executable (e.g., `ruby`, `go`, `rustc`)
- `var` — env var to store resolved path
- `fallback` — absolute path to use if `which` fails

### `command`

Runs a resolve command and uses stdout as the interpreter path:

```yaml
env_config:
  interpreter:
    type: command
    resolve_cmd: ["rbenv", "which", "ruby"]
    var: RYE_RUBY
    fallback: ruby
```

**When:** You need to resolve the interpreter dynamically via a version manager or custom script (pyenv, nvm, rbenv, asdf, etc.).

**Config fields:**
- `resolve_cmd` — command + args to execute; stdout is used as the resolved binary path
- `var` — env var to store resolved path
- `fallback` — fallback if the resolve command fails

## Step 3: Static Environment Variables

Set env vars that the tool needs:

```yaml
env_config:
  env:
    RUBY_ENV: production
    BUNDLE_GEMFILE: "{anchor_path}/Gemfile"
    GEM_PATH: "{anchor_path}/vendor/bundle"
```

Supports template variables (`{anchor_path}`, `{project_path}`, etc.) and env var expansion (`${OTHER_VAR:-default}`).

## Step 4: Anchor Configuration

If your language has module resolution (dependencies, imports), configure anchoring:

```yaml
anchor:
  enabled: true
  mode: auto                    # auto, always, or never
  markers_any: ["Gemfile", "Rakefile"]  # Root markers
  root: tool_dir                # tool_dir, tool_parent, or project_path
  lib: lib/ruby                 # Relative subdir
  env_paths:
    RUBYLIB:
      prepend: ["{anchor_path}"]
    BUNDLE_GEMFILE:
      prepend: ["{anchor_path}/Gemfile"]
```

**Anchor resolution:**
1. Search from tool directory upward for any marker file
2. Set `anchor_path` to the directory where first marker is found
3. Prepend `anchor_path` to `RUBYLIB` and other paths
4. This enables multi-file tool dependencies

**Modes:**
- `auto` — Check for markers; if found, anchor; else skip
- `always` — Always anchor (fails if markers not found)
- `never` — Disable anchoring

## Step 5: Dependency Verification

Optionally verify all dependencies before execution:

```yaml
verify_deps:
  enabled: true
  scope: anchor                 # anchor, tool_dir, tool_siblings, tool_file
  recursive: true
  extensions: [".rb", ".yaml", ".yml", ".json"]
  exclude_dirs: ["vendor", ".git", ".bundle"]
```

When enabled, before execution the runtime walks the specified scope and verifies every matching file:
- File content hash matches signature
- File is signed
- No symlink escapes

Any mismatch raises `IntegrityError` and halts execution.

## Step 6: Execution Config

Define how to invoke the tool:

```yaml
config:
  command: "${RYE_RUBY}"
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 300
```

**Fields:**
- `command` — Interpreter binary (typically from `env_config.interpreter.var`)
- `args` — Array of arguments to pass, supporting template variables
- `timeout` — Execution timeout in seconds
- `cwd` — Optional working directory (defaults to `tool_dir`)

**Template variables available:**

| Variable | Source | Description |
|----------|--------|-------------|
| `{tool_path}` | Tool file | Absolute path to tool |
| `{tool_dir}` | Tool directory | Directory containing tool |
| `{params_json}` | Parameters | JSON string of validated params |
| `{project_path}` | Project root | Project root path |
| `{anchor_path}` | Anchor result | Module resolution root (if anchor enabled) |
| `{runtime_lib}` | Anchor config | Runtime lib path (if anchor enabled) |
| `{user_space}` | Executor | User space path |
| `{system_space}` | Executor | System space path |

## Step 7: Parameter Schema (Optional)

Define what parameters this runtime accepts (for validation and documentation):

```yaml
config_schema:
  type: object
  properties:
    script:
      type: string
      description: Script file path
    args:
      type: array
      items:
        type: string
      description: Script arguments
  required:
    - script
```

This schema is informational — actual validation is done at the tool level via `CONFIG_SCHEMA`.

## Complete Example: Ruby Runtime

```yaml
# rye:signed:TIMESTAMP:HASH:SIG:FP
version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes/ruby
description: "Ruby runtime executor - executes Ruby scripts with Bundler support"

env_config:
  interpreter:
    type: system_binary
    binary: ruby
    var: RYE_RUBY
    fallback: /usr/bin/ruby
  env:
    RUBY_ENV: production

anchor:
  enabled: true
  mode: auto
  markers_any: ["Gemfile", "Rakefile"]
  root: tool_dir
  lib: lib/ruby
  env_paths:
    RUBYLIB:
      prepend: ["{anchor_path}", "{anchor_path}/lib/ruby"]
    BUNDLE_GEMFILE:
      set: "{anchor_path}/Gemfile"

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".rb", ".yaml", ".yml", ".gemspec"]
  exclude_dirs: ["vendor", ".git", ".bundle", ".venv"]

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
      description: Ruby file path
  required:
    - script
```

## Tool Implementation (Ruby)

Tools using your custom runtime declare it via `__executor_id__`:

```ruby
# rye:signed:TIMESTAMP:HASH:SIG:FP
"""
Tool description
"""

__version__ = "1.0.0"
__tool_type__ = "ruby"
__executor_id__ = "rye/core/runtimes/ruby/ruby"
__category__ = "category/path"
__tool_description__ = "What this does"

def execute(params, project_path)
  # Parse params (passed as --params JSON)
  name = params["name"]
  
  # Do work
  output = "Processed #{name}"
  
  { success: true, output: output }
end

if __FILE__ == $0
  require 'json'
  require 'optparse'
  
  params_json = nil
  project_path = nil
  
  OptionParser.new do |opts|
    opts.on("--params JSON") { |v| params_json = v }
    opts.on("--project-path PATH") { |v| project_path = v }
  end.parse!
  
  params = JSON.parse(params_json || "{}")
  result = execute(params, project_path)
  puts JSON.generate(result)
end
```

## Registration

1. Save your runtime YAML to `.ai/tools/<category>/<name>.yaml`
2. Ensure `tool_type: runtime` is set
3. Sign it: `rye_sign(item_type="tool", item_id="<category>/<name>")`
4. Tools now reference it: `__executor_id__ = "<category>/<name>"`

## Validation Checklist

- [ ] Runtime YAML saved to correct path
- [ ] `tool_type` is `runtime`
- [ ] `executor_id` points to a valid primitive
- [ ] `env_config.interpreter` has all required fields
- [ ] `config.command` uses `${RESOLVER_VAR}` from interpreter config
- [ ] `config.args` uses template variables correctly
- [ ] `config.timeout` is reasonable (300–600 for most)
- [ ] `anchor` configuration matches language module system
- [ ] `verify_deps` extensions match source file types
- [ ] YAML is well-formed (test with online YAML validator)
- [ ] Signed: `rye_sign(item_type="tool", item_id="...")`
- [ ] Test with simple tool using `__executor_id__`

## Debugging

Run with `RYE_DEBUG=1` to see:
- Interpreter resolution steps
- Template variable substitution
- Environment variable values
- Anchor discovery process

Common issues:

| Problem | Check |
|---------|-------|
| "Command not found" | Verify `binary` name is in system PATH; check `fallback` |
| "Module not found" | Verify `anchor` and `env_paths` are configured correctly |
| "Parameter parsing fails" | Verify tool receives `--params` and `--project-path` args |
| "Signature verification fails" | Re-sign the tool: `rye_sign(item_type="tool", item_id="...")` |
