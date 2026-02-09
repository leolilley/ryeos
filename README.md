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
      "command": "rye",
      "args": ["serve"]
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

1. Call `mcp__rye__search` with your query
2. Receive matching results from the registry
3. Call `mcp__rye__load` to pull the directive into your project
4. Call `mcp__rye__execute` with the PR details

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

**Mandatory fields:**

- `name` and `version` (root attributes)
- `description`, `category`, `author` (in `<metadata>`)
- `<process>` with one or more `<step>` elements
- `<success_criteria>` with one or more `<criterion>` elements
- `<outputs>` with one or more `<output>` elements

For advanced features (model tiers, permissions, cost tracking), see `create_advanced_directive`.

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
Call mcp__rye__execute with:
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

When you search from your agent, results from the registry appear alongside your local items.

### Publishing to the Registry

Use the MCP execute tool to publish items:

```
Call mcp__rye__execute with:
- item_type: "directive"
- action: "publish"
- item_id: "code_review"
- parameters: {"version": "1.0.0"}
```

## Permissions and Security

Rye enforces permissions at multiple levels:

1. **MCP Protocol**: Controls what the external agent can access
2. **Directive Permissions**: Declared in the directive's `<permissions>` section
3. **Runtime Scoping**: Each tool runs with only the permissions it needs

Permission declarations use XML elements:

```xml
<permissions>
  <read resource="filesystem" path="**/*" />
  <write resource="filesystem" path="tests/**" />
  <execute resource="shell" action="pytest" />
  <execute resource="rye" action="search" />
</permissions>
```

This layered approach ensures that even if a directive from the registry is malicious, it can only access resources its permissions allow.

## Project Structure

When you use Rye in a project, it creates a `.ai/` directory:

```
your-project/
├── .ai/
│   ├── directives/      # XML workflow definitions
│   ├── tools/           # Tool configurations and scripts
│   └── knowledge/       # Domain information and patterns
├── src/
└── README.md
```

This structure is committed to your repository, making your AI workflows part of your codebase.

## Why This Matters

Traditional agent environments treat prompts, tools, and knowledge as transient. Rye makes them:

- **Persistent**: Stored alongside your code
- **Portable**: Works across different agent clients
- **Collaborative**: Teams can share and improve workflows
- **Composable**: Combine multiple directives into complex pipelines
- **Inspectable**: You can read and modify everything

Instead of starting from scratch in each agent environment, you build up a library of reusable components that improve over time.

## Examples

- **Research Pipeline**: A directive that spawns parallel agents to analyze multiple sources, extracts key findings, and synthesizes a report
- **Code Review System**: Automated PR review checking security, performance, and style
- **Data Processing Pipeline**: Extract, transform, validate, and load data with provenance tracking
- **Content Factory**: Research, outline, draft, edit, and publish with multi-stage approval

## License

MIT

## Contributing

Contributions welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.
