# Rye OS

Rye OS is an MCP (Model Context Protocol) server that enables interoperability across different AI agents and their associated workflows, tools, and knowledge.

## Problem

Different AI agents (Claude Desktop, Cursor, Windsurf, etc.) each operate in isolation. Workflows, tools, and knowledge created in one agent cannot be easily reused or shared with another. This leads to:

- **Reinventing the wheel**: Every agent environment re-solves the same problems independently
- **No portability**: You can't take your setup from one agent to another
- **Fragmented tooling**: Tools developed for one agent can't be used by others
- **Lost knowledge**: Learnings and patterns aren't captured in a reusable way

## Solution

Rye OS provides a unified data model and runtime for agent workflows, tools, and knowledge. It treats all of these as structured data that can be:

- **Shared** across different agent environments
- **Versioned** like any other code
- **Discovered** through a searchable registry
- **Composed** together into complex workflows

## Architecture

Rye OS exposes **4 MCP tools** that operate on **3 types of data**:

### The 4 MCP Tools

| Tool      | Purpose                                         |
| --------- | ----------------------------------------------- |
| `search`  | Find directives, tools, or knowledge by query   |
| `load`    | Load an item into your local project            |
| `execute` | Run a directive or tool                         |
| `sign`    | Cryptographically sign an item for distribution |

### The 3 Data Types

- **Directives** — XML-based workflow definitions that specify HOW to accomplish a task
- **Tools** — Executable code that performs specific actions (Python, JavaScript, YAML, Bash, etc.)
- **Knowledge** — Structured information, patterns, and learnings (markdown with YAML frontmatter)

All three types are stored as data files in your project (`.ai/` directory) and can be shared via a registry.

### How It Works

When an agent wants to accomplish a task:

1. **Search**: Find relevant directives, tools, or knowledge
2. **Load**: Bring them into the local project space
3. **Execute**: Run directives which orchestrate tools and knowledge

Because everything is data, you can:

- Store workflows in your project repository
- Share them with a team
- Version them alongside your code
- Pull new versions from the registry

## Installation

```bash
pip install rye-os
```

### Connect to Your Agent

For Claude Desktop, add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "rye": {
      "command": "rye"
    }
  }
}
```

For other MCP-compatible agents (Cursor, Windsurf, Gemini), see their documentation for MCP server configuration.

## Usage

### Basic Example

Prompt your agent:

```
"Search for a code review directive and use it to review this PR"
```

Your agent will:

1. Call `search` with your query
2. Receive matching results from the registry
3. Call `load` to pull the directive into your project
4. Call `execute` with the PR details

The directive is now in your project (`.ai/directives/`) and can be reused without searching again.

### Creating Directives

Directives are **XML-formatted** markdown files. Here's a minimal example:

```xml
<directive name="deploy_app" version="1.0.0">
  <metadata>
    <description>Deploy application to production environment.</description>
    <category>workflows</category>
    <author>devops-team</author>
  </metadata>

  <process>
    <step name="build">
      <description>Build the application</description>
      <action><![CDATA[
npm run build
      ]]></action>
    </step>

    <step name="deploy">
      <description>Deploy to production</description>
      <action><![CDATA[
npm run deploy:production
      ]]></action>
    </step>

    <step name="verify">
      <description>Check deployment health</description>
      <action><![CDATA[
Verify application is responding at https://app.example.com
      ]]></action>
    </step>
  </process>

  <success_criteria>
    <criterion>Build completes successfully</criterion>
    <criterion>Application is accessible and responding</criterion>
  </success_criteria>

   <outputs>
     <output name="deployment_url">Final URL of deployed application</output>
     <output name="status">Deployment status (success/failed)</output>
     <output name="logs">Build and deployment log file path</output>
   </outputs>
</directive>
```

**Required fields:**

- `name` and `version` (root attributes)
- `description`, `category`, `author` (in `<metadata>`)
- `<process>` with one or more `<step>` elements
- `<success_criteria>` with one or more `<criterion>` elements
- `<outputs>` with one or more `<output>` elements

**Optional advanced fields** (required for agent thread execution):

- `<model>` - Model tier, fallback, and parallel execution settings
- `<permissions>` - Resource access declarations (required for agent threads)
- `<cost>` - Resource usage tracking and budget management
- `<context>` - Relationships, dependencies, and related files
- `<hooks>` - Conditional actions for events and deviations

For advanced features including hooks, context relationships, and model tiers, see `create_advanced_directive`.

### Creating Tools

Tools are executable scripts in any language. Here's a simple Python tool example:

```python
# .ai/tools/utilities/deploy-service.py
"""
Tool: deploy-service
Version: 1.0.0
Tool Type: python
Executor: python_runtime
Category: utilities
Description: Deploy service to Kubernetes cluster
"""

def main(**kwargs):
    """Execute deployment with provided parameters"""
    service_name = kwargs.get('service_name')
    replicas = kwargs.get('replicas', 3)

    # Deployment logic here
    print(f"Deploying {service_name} with {replicas} replicas")
    return {"status": "success", "service": service_name}
```

Use the MCP execute tool to register tools:

```
Call execute with:
- item_type: "tool"
- action: "create"
- item_id: "deploy-service"
- parameters: {...}
```

### Managing Knowledge

Knowledge entries capture patterns, learnings, or domain information using YAML frontmatter:

```markdown
---
id: python-async-patterns
title: Python Async Best Practices
category: patterns/async
version: "1.0.0"
author: user
created_at: 2026-02-05T00:00:00Z
tags:
  - python
  - async
  - best-practices
extends:
  - python-basics
references:
  - https://docs.python.org/3/library/asyncio.html
---

# Python Async Best Practices

## When to Use Async

- I/O-bound operations
- Network requests
- File operations

## Common Pitfalls

- Don't use async for CPU-bound work
- Always gather coroutines, don't await sequentially
```

## Registry

The registry is a centralized store of shared directives, tools, and knowledge. It enables:

- **Discovery**: Find solutions others have already created
- **Validation**: Items are cryptographically signed
- **Versioning**: Track changes and updates
- **Community**: Share improvements back

When you search from your agent, results from the local registry appear alongside your local items.

## Agent Tool and Thread Spawning

Rye OS includes an advanced **Agent Tool** that enables recursive AI agents with tight control through metadata:

### The Agent Tool

The agent tool (`.ai/tools/rye/agent/`) provides:

- **Thread Spawning**: Create parallel AI threads that can run directives independently
- **MCP Integration**: Spawned agents receive the same Rye MCP tools, enabling full recursion
- **Safety Harness**: Built into the agent tool with thread validation and isolation
- **Resource Control**: Managed through directive metadata (permissions, cost, model tiers)

### Spawning Agent Threads

Use the agent tool to spawn parallel AI threads:

```bash
# Spawn a thread for a specific task
rye_execute(
  item_type="tool",
  item_id="rye/agent/threads/spawn_thread",
  parameters={
    "thread_id": "research-analysis",
    "directive_name": "analyze-market-trends",
    "project_path": "/path/to/project"
  }
)
```

### Recursive Agent Architecture

1. **Main Agent** receives a task and searches for appropriate directives
2. **Spawned Threads** get their own Rye MCP client access
3. **Thread Control** through metadata:
   - `<permissions>`: Limited to thread-specific resources
   - `<cost>`: Budget tracking per thread
   - `<model>`: Tier selection for thread reasoning complexity
   - `<context>`: Thread relationships and dependencies

### Thread Registry

Agent threads are automatically registered and can be:

- Monitored via `read_transcript`
- Controlled via `thread_registry`
- Terminated individually with validation

### Publishing to the Registry

Use the MCP execute tool to publish items:

```
Call execute with:
- item_type: "directive"
- action: "publish"
- item_id: "code_review"
- parameters: {"version": "1.0.0"}
```

## Permissions and Security

Rye enforces permissions at multiple levels:

1. **MCP Protocol**: Controls what the external agent can access
2. **Directive Permissions**: Declared in the directive's `<permissions>` section
3. **Thread Isolation**: Each spawned agent thread operates with its own permission set
4. **Runtime Scoping**: Each tool runs with only the permissions it needs

### Permission Declarations

Permissions follow a hierarchical structure under `<permissions>` root. Each permission type (execute, search, load, sign) can contain specific resource types and capabilities. Each permission level can independently access tools, directives, or knowledge.

```xml
<permissions>
  <execute>
    <tool>rye.file-system.fs_write</tool>
    <tool>rye.agent.*</tool>
  </execute>

  <search>*</search>

  <load>
    <tool>rye.shell.*</tool>
    <directive>workflow/*</directive>
  </load>

  <sign>*</sign>
</permissions>
```

**Permission hierarchy**:

- God mode: `<permissions>*</permissions>` - unlimited access to everything
- Execute only: `<permissions><execute>*</execute></permissions>` - can execute anything but cannot search, load, or sign
- Search only: `<permissions><search>*</search></permissions>` - can search for any item but cannot execute
- Partial access: List specific tools or patterns per permission type

**Common patterns**:

```xml
<!-- File system access only -->
<permissions>
  <execute>
    <tool>rye.file-system.*</tool>
  </execute>
</permissions>

<!-- Full execute access -->
<permissions>
  <execute>*</execute>
</permissions>

<!-- Search everything -->
<permissions>
  <search>*</search>
</permissions>

<!-- Full access to execute, search, and load -->
<permissions>
  <execute>*</execute>
  <search>*</search>
  <load>*</load>
</permissions>

<!-- Custom access for each permission type -->
<permissions>
  <execute>
    <tool>rye.file-system.*</tool>
    <tool>rye.agent.threads.spawn_thread</tool>
  </execute>
  <search>
    <directive>analysis/*</directive>
    <tool>rye.registry.*</tool>
  </search>
  <load>
    <tool>rye.*</tool>
  </load>
  <sign>
    <tool>scripts.*</tool>
  </sign>
</permissions>
```

See [permission-format-change.md](docs/permission-format-change.md) for detailed migration guide.

### Thread Safety and Control

The agent tool provides built-in safety features:

- **Thread Validation**: Thread ID sanitization and uniqueness checking
- **Resource Scoping**: Each thread gets its own permission context
- **Budget Tracking**: Individual cost limits per spawned thread
- **Termination Control**: Safe thread termination with cleanup
- **Registry Integration**: Thread state tracking and management

This layered approach ensures that even if a directive from the registry is malicious or spawns threads, all operations are contained within their declared permission boundaries.

## Project Structure

When you use Rye in a project, it creates a `.ai/` directory:

```
your-project/
├── .ai/                # Directory should be git ignored
│   ├── directives/      # XML workflow definitions (local workspace)
│   ├── tools/           # Tool configurations and scripts (local workspace)
│   └── knowledge/       # Domain information and patterns (local workspace)
├── src/
└── README.md
```

The `.ai/` directory should be **git ignored** to keep your workflows private and separate from your codebase. This is by design - versioning and sharing happens through the registry, not git.

## Why This Matters

Traditional agent environments treat prompts, tools, and knowledge as transient. Rye makes them:

- **Persistent**: Stored in your local workspace alongside your code
- **Portable**: Works across different agent clients through the registry
- **Collaborative**: Teams share and improve workflows via the registry
- **Composable**: Combine multiple directives into complex pipelines
- **Inspectable**: You can read and modify everything locally

Instead of committing AI workflows to your git history, you build up a library of reusable components that improve over time and are shared through the registry. This keeps your codebase clean while enabling powerful AI workflows.

## Examples

### Threaded Research Pipeline

A directive that spawns parallel agent threads to analyze multiple sources, extracts key findings, and synthesizes a report with full recursion.

### Multi-Agent Code Review

Spawn specialized agent threads for:

- Security analysis with dedicated permissions
- Performance optimization with cost limits
- Style compliance with specific model tiers
- Auto-comment generation with output templates

### Distributed Data Processing

Extract, transform, validate, and load data across multiple agent threads with provenance tracking and individual resource controls.

### Content Factory Pipeline

Sequential spawning of specialized agent threads:

- Research agents with broad access
- Outline agents with structural validation
- Draft agents with style constraints
- Edit agents with limited scope
- Approval agents with elevated permissions

## Agent Thread Examples

### Parallel Code Analysis with Scoped Permissions

```xml
<directive name="parallel-code-analysis" version="1.0.0">
  <metadata>
    <description>Analyze code in parallel across multiple dimensions with isolated thread permissions</description>
    <category>analysis</category>
    <author>team</author>

    <model tier="orchestrator" parallel="true">
      Spawn parallel agents for comprehensive analysis
    </model>

    <permissions>
      <execute>
        <tool>rye.agent.threads.spawn_thread</tool>
        <tool>rye.agent.threads.thread_registry</tool>
        <tool>rye.agent.threads.read_transcript</tool>
      </execute>
    </permissions>

    <cost>
      <context estimated_usage="high" turns="10" spawn_threshold="3">
        50000
      </context>
    </cost>
  </metadata>

  <process>
    <step name="spawn-security-analysis">
      <description>Spawn security-focused thread with scoped permissions</description>
      <action><![CDATA[
        Spawn agent thread with:
        - Thread ID: security-analysis
        - Directive: security-vulnerability-scanner
        - Limited permissions to source code only
        - Cost tracking within budget
      ]]></action>
    </step>

    <step name="spawn-performance-analysis">
      <description>Spawn performance-focused thread</description>
      <action><![CDATA[
        Spawn agent thread with:
        - Thread ID: performance-analysis
        - Directive: code-performance-optimizer
        - Read access to tests and benchmarks
        - Different model tier for performance reasoning
      ]]></action>
    </step>

    <step name="coordinate-results">
      <description>Wait for all threads and synthesize results</description>
      <action><![CDATA[
        Load transcript from all spawned threads
        Cross-reference findings between threads
        Generate unified analysis report
      ]]></action>
    </step>
  </process>
</directive>
```

### God Mode Thread

```xml
<directive name="root-directive" version="1.0.0">
  <metadata>
    <description>Root directive with unlimited access for recursive operations</description>
    <category>meta</category>
    <author>system</author>

    <model tier="orchestrator">
      Highest level directive with god-mode access
    </model>

    <permissions>*</permissions>

    <cost>
      <context estimated_usage="high" turns="20" spawn_threshold="5">
        100000
      </context>
    </cost>
  </metadata>

  <process>
    <!-- Content here... -->
  </process>
</directive>
```

### Execute-Only Thread

```xml
<directive name="executor-thread" version="1.0.0">
  <metadata>
    <description>Thread with execute permissions only (cannot search or load)</description>
    <category>executor</category>
    <author>system</author>

    <model tier="general">
      Execute directives but cannot search or load
    </model>

    <permissions><execute>*</execute></permissions>

    <cost>
      <context estimated_usage="medium" turns="10" spawn_threshold="2">
        20000
      </context>
    </cost>
  </metadata>

  <process>
    <!-- Content here... -->
  </process>
</directive>
```

### Search-Only Thread

```xml
<directive name="searcher-thread" version="1.0.0">
  <metadata>
    <description>Thread can search for anything but cannot execute</description>
    <category>searcher</category>
    <author>system</author>

    <model tier="general">
      Search for directives and tools but cannot execute
    </model>

    <permissions><search>*</search></permissions>

    <cost>
      <context estimated_usage="low" turns="3" spawn_threshold="1">
        5000
      </context>
    </cost>
  </metadata>

  <process>
    <!-- Content here... -->
  </process>
</directive>
```

### File System Only Thread

```xml
<directive name="file-processor" version="1.0.0">
  <metadata>
    <description>Process files with limited file system access</description>
    <category>file-system</category>
    <author>developer</author>

    <model tier="general">
      Standard processing with file operations only
    </model>

    <permissions>
      <execute>
        <tool>rye.file-system.fs_read</tool>
        <tool>rye.file-system.fs_write</tool>
        <tool>rye.file-system.fs_exists</tool>
      </execute>
    </permissions>

    <cost>
      <context estimated_usage="medium" turns="5" spawn_threshold="2">
        10000
      </context>
    </cost>
  </metadata>

  <process>
    <!-- Content here... -->
  </process>
</directive>
```

## License

MIT

## Contributing

Contributions welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.
