# Rye OS Item Types Reference

Complete metadata specifications for the three primary item types in Rye OS: **Directives**, **Tools**, and **Knowledge**.

## Quick Overview

| Item Type     | Format                         | Purpose                                | Metadata                                         |
| ------------- | ------------------------------ | -------------------------------------- | ------------------------------------------------ |
| **Directive** | XML in Markdown                | Workflow orchestration and automation  | [directive-metadata.md](./directive-metadata.md) |
| **Tool**      | YAML                           | Executable operations and scripts      | [tool-metadata.md](./tool-metadata.md)           |
| **Knowledge** | Markdown with YAML frontmatter | Learnings, patterns, and documentation | [knowledge-metadata.md](./knowledge-metadata.md) |

---

## Directives

**What:** Workflow automation definitions that orchestrate tasks, call tools, and make decisions.

**Format:** XML embedded in Markdown files

```
.ai/directives/
├── core/
│   ├── deploy.md
│   └── test.md
└── implementation/
    └── my-directive.md
```

**Key Metadata:**

- `name`, `version` - Identity
- `description`, `category`, `author` - Organization
- `model` - LLM tier selection
- `permissions` - Capability declarations
- `hooks` - Event handlers

**See:** [Directive Metadata Reference](./directive-metadata.md)

---

## Tools

**What:** Executable operations that directives can call (scripts, primitives, runtimes).

**Format:** YAML files with embedded JSON schemas

```
.ai/tools/
├── core/
│   ├── deploy-service.yaml
│   └── validate-config.yaml
└── custom/
    └── my-tool.yaml
```

**Key Metadata:**

- `tool_id`, `version` - Identity
- `description`, `category` - Organization
- `parameters` - Input specification
- `outputs` - Output specification
- `requires` - Capability requirements
- `executor_id` - Runtime selection

**See:** [Tool Metadata Reference](./tool-metadata.md)

---

## Knowledge

**What:** Documentation, patterns, learnings, specifications, and guides stored in searchable knowledge base.

**Format:** Markdown files with YAML frontmatter

```
.ai/knowledge/
├── architecture/
│   ├── patterns/
│   │   └── retry-logic.md
│   └── specifications/
│       └── api-design.md
├── security/
│   └── authentication/
│       └── oauth2-patterns.md
└── learnings/
    └── debugging-techniques.md
```

**Key Metadata:**

- `id`, `title`, `version` - Identity
- `entry_type`, `category` - Organization
- `tags`, `references` - Discovery
- `stability`, `difficulty` - Context
- `validated_at` - Maintenance

**See:** [Knowledge Metadata Reference](./knowledge-metadata.md)

---

## Integration Example

A complete system bringing all three together:

### 1. Define the Tool

```yaml
# .ai/tools/deployment/deploy-service.yaml
tool_id: deploy-kubernetes
tool_type: script
version: "1.0.0"
description: "Deploy service to Kubernetes cluster"
executor_id: python_runtime

parameters:
  - name: service_name
    type: string
    required: true
  - name: image
    type: string
    required: true
```

### 2. Document the Knowledge

```markdown
## <!-- .ai/knowledge/deployment/kubernetes-patterns.md -->

id: k8s-deployment-patterns
title: Kubernetes Deployment Patterns
entry_type: pattern
category: deployment
version: "1.0.0"
tags: [kubernetes, deployment, best-practices]

---

# Kubernetes Deployment Patterns

Best practices for deploying services...
```

### 3. Use in Directive

```xml
<!-- .ai/directives/workflows/deploy-prod.md -->
<directive name="deploy-production" version="1.0.0">
  <metadata>
    <description>Deploy to production with health checks</description>
    <category>workflows</category>
    <permissions>
      <execute resource="tool" id="deploy-kubernetes"/>
    </permissions>
  </metadata>

  <process>
    <step name="deploy">
      <action>
        execute(tool, run, deploy-kubernetes, {
          service_name: "api-service",
          image: "ghcr.io/my-org/api:latest"
        })
      </action>
    </step>
  </process>
</directive>
```

---

## Key Concepts

### Metadata Fields

All three item types use metadata for:

1. **Identity** - Unique ID, version, name
2. **Organization** - Category, tags, type classification
3. **Context** - Description, author, purpose
4. **Capabilities** - Permissions, requires, dependencies
5. **Maintenance** - Version, validated_at, stability
6. **Relationships** - References, extends, used_by

### Naming Conventions

Use **kebab-case** for all identifiers:

```
✅ Correct:
  - directive: create-project-structure
  - tool: deploy-kubernetes
  - knowledge: oauth2-implementation

❌ Incorrect:
  - Create_Project_Structure
  - DeployKubernetes
  - oauth2Implementation
```

### Versioning

All items use **semantic versioning** (X.Y.Z):

- **X** - Major (breaking changes)
- **Y** - Minor (backwards-compatible additions)
- **Z** - Patch (bug fixes)

Example progression:

```
1.0.0 → 1.1.0 (added new parameter)
      → 2.0.0 (changed parameter format)
      → 2.0.1 (fixed bug)
```

### Categories

Organize items hierarchically:

```
architecture/patterns
architecture/specifications
security/authentication
security/encryption
deployment/kubernetes
deployment/aws
tools/testing
tools/validation
```

---

## Quick Reference

### Creating Items

**Directive:**

1. Create `.md` file in `.ai/directives/`
2. Add `<directive>` XML block with metadata
3. Define process steps
4. Validate with tooling

**Tool:**

1. Create `.yaml` file in `.ai/tools/`
2. Define metadata, parameters, outputs
3. Specify executor and requirements
4. Document with examples

**Knowledge:**

1. Create `.md` file in `.ai/knowledge/`
2. Add YAML frontmatter with metadata
3. Write markdown content
4. Link to related items

### Searching Items

All items are searchable by:

- `id` / `name` - Exact match
- `title` - Title search
- `tags` - Tag-based discovery
- `category` - Hierarchical browsing
- `description` - Full-text search
- `references` - Knowledge graph navigation

### Validation

Each item type has validation rules:

**Directives:**

- Required: name, version, description, category, author, model
- Semantic version format
- Valid category enum
- Valid model tier

**Tools:**

- Required: tool_id, tool_type, version, description, executor_id
- Valid parameter types (string, integer, object, etc.)
- Capability names in dotted notation
- Parameter validation schema

**Knowledge:**

- Required: id, title, entry_type, category, version
- Valid entry_type enum
- Valid stability/difficulty enums
- ISO 8601 datetime format
- Correct content hash

---

## See Also

- [Directive Metadata Reference](./directive-metadata.md) - Complete directive specification
- [Tool Metadata Reference](./tool-metadata.md) - Complete tool specification
- [Knowledge Metadata Reference](./knowledge-metadata.md) - Complete knowledge specification
- [Rye OS Principles](../principles.md) - Design philosophy
- [MCP Server Documentation](../mcp-server.md) - Protocol implementation
