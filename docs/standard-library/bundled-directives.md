```yaml
id: bundled-directives
title: "Bundled Directives"
description: The 5 directives that ship with Rye OS — item creation and thread management
category: standard-library
tags: [directives, bundled, create, threaded, standard-library]
version: "1.0.0"
```

# Bundled Directives

Rye OS ships five directives in the standard library. Four are **user-facing creation directives** for scaffolding new items. One is **infrastructure** used internally by the thread system.

| Directive                   | Category       | Purpose                                              |
| --------------------------- | -------------- | ---------------------------------------------------- |
| `create_directive`          | User-facing    | Scaffold a new directive                             |
| `create_tool`               | User-facing    | Scaffold a new tool                                  |
| `create_knowledge`          | User-facing    | Scaffold a new knowledge entry                       |
| `create_threaded_directive` | User-facing    | Scaffold a directive with thread execution support   |
| `thread_summary`            | Infrastructure | Summarize thread conversations for context carryover |

All five live in system space and are available to every project without setup.

---

## 1. `rye/core/create_directive` (v3.0.0)

**Purpose:** Create a new directive file with proper metadata, validate it, and sign it.

### Configuration

| Setting    | Value                                    |
| ---------- | ---------------------------------------- |
| Model      | `claude-3-5-haiku-20241022` (haiku tier) |
| Max turns  | 6                                        |
| Max tokens | 4,096                                    |

### Permissions

- **Search:** directives (`*`)
- **Execute:** file-system tools (`rye.file-system.*`)
- **Sign:** directives (`*`)

### Inputs

| Name            | Type   | Required | Description                                                                 |
| --------------- | ------ | -------- | --------------------------------------------------------------------------- |
| `name`          | string | ✅       | Directive name in `snake_case` (e.g., `deploy_app`, `create_component`)     |
| `category`      | string | ✅       | Directory path relative to `.ai/directives/` (e.g., `workflows`, `testing`) |
| `description`   | string | ✅       | What the directive does — be specific and actionable                        |
| `process_steps` | string | ❌       | Brief summary of process steps (bullet points)                              |

### Process

1. **Check duplicates** — Searches for existing directives with a similar name. Stops if a duplicate exists.
2. **Validate inputs** — Confirms `name` is snake_case alphanumeric, `category` and `description` are non-empty.
3. **Write directive file** — Generates the Markdown file at `.ai/directives/{category}/{name}.md` with:
   - Signature comment placeholder at the top
   - Markdown title and description
   - XML fenced block containing metadata, inputs, and outputs
   - Process steps after the fence (informed by `process_steps` if provided)
4. **Sign** — Validates and signs the new directive file.

### Outputs

| Name             | Description                                   |
| ---------------- | --------------------------------------------- |
| `directive_path` | Path to the created directive file            |
| `signed`         | Whether the directive was successfully signed |

### Invocation

```python
rye_execute(
    item_type="directive",
    item_id="rye/core/create_directive",
    parameters={
        "name": "deploy_app",
        "category": "workflows",
        "description": "Deploy the application to production with health checks"
    }
)
```

With optional process steps:

```python
rye_execute(
    item_type="directive",
    item_id="rye/core/create_directive",
    parameters={
        "name": "run_migrations",
        "category": "database",
        "description": "Run pending database migrations safely",
        "process_steps": "- Check for pending migrations\n- Create backup\n- Apply migrations\n- Verify schema"
    }
)
```

---

## 2. `rye/core/create_tool` (v3.0.0)

**Purpose:** Create a new tool file with correct metadata headers and parameter schema, then sign it.

### Configuration

| Setting    | Value                                    |
| ---------- | ---------------------------------------- |
| Model      | `claude-3-5-haiku-20241022` (haiku tier) |
| Max turns  | 8                                        |
| Max tokens | 4,096                                    |

### Permissions

- **Execute:** file-system tools (`rye.file-system.*`)
- **Search:** tools (`*`)
- **Sign:** tools (`*`)

### Inputs

| Name          | Type   | Required | Description                                                                          |
| ------------- | ------ | -------- | ------------------------------------------------------------------------------------ |
| `tool_name`   | string | ✅       | Unique identifier in `kebab-case` (e.g., `deploy-service`, `validate-jwt`)           |
| `category`    | string | ✅       | Directory path relative to `.ai/tools/` (e.g., `utilities`, `deployment/kubernetes`) |
| `tool_type`   | string | ✅       | Type of tool: `python`, `yaml`, `bash`, or `javascript`                              |
| `description` | string | ✅       | What the tool does                                                                   |
| `parameters`  | string | ❌       | Comma-separated parameter definitions: `name:type:required`                          |

### Process

1. **Check existing** — Searches for tools with a similar name to avoid duplicates.
2. **Write tool file** — Determines the file extension from `tool_type` (`python`→`.py`, `yaml`→`.yaml`, `bash`→`.sh`, `javascript`→`.js`) and generates the file at `.ai/tools/{category}/{tool_name}.{ext}`.

   **For Python tools**, the generated file includes:

   ```python
   __version__ = "1.0.0"
   __tool_type__ = "python"
   __executor_id__ = "rye/core/runtimes/python/script"
   __category__ = "utilities"
   __tool_description__ = "What the tool does"

   CONFIG_SCHEMA = {
       "param_name": {"type": "string", "required": True, "description": "..."}
   }

   async def execute(params: dict, project_path: str) -> dict:
       # Implementation
       ...
   ```

   **For YAML tools**, the generated file includes:

   ```yaml
   tool_id: my-tool
   tool_type: yaml
   version: "1.0.0"
   executor_id: rye/core/runtimes/...
   category: utilities
   description: What the tool does
   parameters:
     - name: param_name
       type: string
       required: true
       description: "..."
   config:
     # Tool-specific settings
   ```

3. **Sign** — Validates metadata and signs the new tool file.

### Outputs

| Name        | Description                              |
| ----------- | ---------------------------------------- |
| `tool_path` | Path to the created tool file            |
| `signed`    | Whether the tool was successfully signed |

### Invocation

```python
rye_execute(
    item_type="directive",
    item_id="rye/core/create_tool",
    parameters={
        "tool_name": "analyze-logs",
        "category": "utilities",
        "tool_type": "python",
        "description": "Analyze log files for errors and patterns"
    }
)
```

With parameters:

```python
rye_execute(
    item_type="directive",
    item_id="rye/core/create_tool",
    parameters={
        "tool_name": "deploy-service",
        "category": "deployment/kubernetes",
        "tool_type": "python",
        "description": "Deploy a service to Kubernetes",
        "parameters": "service_name:string:true, replicas:integer:false, namespace:string:false"
    }
)
```

---

## 3. `rye/core/create_knowledge` (v3.0.0)

**Purpose:** Create a new knowledge entry with YAML frontmatter and Markdown content, then sign it.

### Configuration

| Setting    | Value                                    |
| ---------- | ---------------------------------------- |
| Model      | `claude-3-5-haiku-20241022` (haiku tier) |
| Max turns  | 6                                        |
| Max tokens | 4,096                                    |

### Permissions

- **Execute:** file-system tools (`rye.file-system.*`)
- **Search:** knowledge (`*`)
- **Sign:** knowledge (`*`)

### Inputs

| Name       | Type   | Required | Description                                                                               |
| ---------- | ------ | -------- | ----------------------------------------------------------------------------------------- |
| `id`       | string | ✅       | Unique identifier in `kebab-case` (e.g., `jwt-validation`, `deployment-strategies`)       |
| `title`    | string | ✅       | Human-readable title for the knowledge entry                                              |
| `category` | string | ✅       | Directory path relative to `.ai/knowledge/` (e.g., `security/authentication`, `patterns`) |
| `content`  | string | ✅       | Main Markdown content of the knowledge entry                                              |
| `tags`     | string | ❌       | Comma-separated tags (3–5 recommended, e.g., `jwt, tokens, security`)                     |

### Process

1. **Check duplicates** — Searches for existing knowledge entries with a similar ID.
2. **Write entry** — Generates the file at `.ai/knowledge/{category}/{id}.md` with:
   ```yaml
   ---
   id: jwt-validation
   title: JWT Validation Patterns
   category: security/authentication
   version: "1.0.0"
   author: ryeos
   tags:
     - jwt
     - tokens
     - security
   ---
   ```
   Followed by the Markdown content.
3. **Sign** — Validates and signs the new knowledge entry.

### Outputs

| Name             | Description                               |
| ---------------- | ----------------------------------------- |
| `knowledge_path` | Path to the created knowledge entry file  |
| `signed`         | Whether the entry was successfully signed |

### Invocation

```python
rye_execute(
    item_type="directive",
    item_id="rye/core/create_knowledge",
    parameters={
        "id": "api-patterns",
        "title": "Common API Design Patterns",
        "category": "patterns",
        "content": "## REST Conventions\n\nUse nouns for resources, HTTP verbs for actions...",
        "tags": "api, rest, patterns, design"
    }
)
```

---

## 4. `rye/core/create_threaded_directive` (v2.0.0)

**Purpose:** Create a directive with full thread execution support — model configuration, cost limits, and capability permissions for autonomous thread-based execution via `thread_directive`.

This is the directive you use when you want the result to run autonomously in a thread with budget controls, rather than being executed inline.

### Configuration

| Setting    | Value                                    |
| ---------- | ---------------------------------------- |
| Model      | `claude-3-5-haiku-20241022` (haiku tier) |
| Max turns  | 8                                        |
| Max tokens | 4,096                                    |

### Permissions

- **Execute:** file-system tools (`rye.file-system.*`)
- **Search:** directives (`*`)
- **Load:** directives (`*`)
- **Sign:** directives (`*`)

### Inputs

| Name                 | Type   | Required | Description                                          |
| -------------------- | ------ | -------- | ---------------------------------------------------- |
| `name`               | string | ✅       | Directive name in `snake_case`                       |
| `category`           | string | ✅       | Category path (e.g., `rye/core`, `project/build`)    |
| `description`        | string | ✅       | What the threaded directive does                     |
| `complexity`         | string | ✅       | Complexity level: `simple`, `moderate`, or `complex` |
| `permissions_needed` | string | ✅       | Comma-separated capability strings                   |
| `process_steps`      | string | ❌       | Summary of the steps the directive should perform    |

### Complexity Mapping

The `complexity` input determines default resource limits:

| Complexity | Max Turns | Max Tokens | Max Spend |
| ---------- | --------- | ---------- | --------- |
| `simple`   | 6         | 4,096      | $0.05     |
| `moderate` | 15        | 200,000    | $0.50     |
| `complex`  | 30        | 200,000    | $1.00     |

### Process

1. **Search existing** — Looks for similar directives to avoid duplication and gather patterns.
2. **Load reference** — Loads an example threaded directive as a structural reference.
3. **Determine limits** — Maps the `complexity` input to turn count, token budget, and spend cap.
4. **Write directive** — Generates the file at `.ai/directives/{category}/{name}.md` with:
   - Signature placeholder
   - Markdown title and description
   - XML fenced block with `<model>`, `<limits>`, and `<permissions>` elements
   - Process steps after the fence
   - Permissions parsed from `permissions_needed` into hierarchical XML entries grouped by action (execute, search, load, sign)
5. **Sign** — Validates and signs the directive.

### Outputs

| Name             | Description                                   |
| ---------------- | --------------------------------------------- |
| `directive_path` | Path to the created threaded directive file   |
| `signed`         | Whether the directive was successfully signed |

On success, the result includes the exact `rye_execute` call to run the new threaded directive:

```
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/thread_directive",
    parameters={"directive_name": "{category}/{name}"}
)
```

### Invocation

```python
rye_execute(
    item_type="directive",
    item_id="rye/core/create_threaded_directive",
    parameters={
        "name": "deploy_pipeline",
        "category": "deployment",
        "description": "Deploy the application through the full CI/CD pipeline",
        "complexity": "moderate",
        "permissions_needed": "rye.execute.tool.rye.file-system.*,rye.execute.tool.rye.bash.*,rye.search.directive.*"
    }
)
```

Simple complexity example:

```python
rye_execute(
    item_type="directive",
    item_id="rye/core/create_threaded_directive",
    parameters={
        "name": "lint_project",
        "category": "quality",
        "description": "Run linters and report issues",
        "complexity": "simple",
        "permissions_needed": "rye.execute.tool.rye.bash.*,rye.execute.tool.rye.file-system.read",
        "process_steps": "- Run linter command\n- Parse output\n- Report summary"
    }
)
```

---

## 5. `rye/agent/threads/thread_summary` (v1.0.0)

**Purpose:** Summarize a thread conversation for context carryover during thread resumption. Produces a structured summary that fits within a token budget.

> **Note:** This directive is no longer called automatically by the infrastructure. Summarization is opt-in — directives that want summarization on handoff declare an `after_complete` hook that invokes this directive. You can also invoke it directly.

### Configuration

| Setting    | Value     |
| ---------- | --------- |
| Model      | Fast tier |
| Max turns  | 3         |
| Max tokens | 8,192     |
| Max spend  | $0.02     |

### Permissions

- **Execute:** thread internal tools only (`rye/agent/threads/internal/*`)

### Inputs

| Name                 | Type    | Required | Description                                                  |
| -------------------- | ------- | -------- | ------------------------------------------------------------ |
| `transcript_content` | string  | ✅       | The full or partial transcript content to summarize          |
| `directive_name`     | string  | ✅       | Name of the directive the thread was executing               |
| `max_summary_tokens` | integer | ❌       | Target maximum tokens for the summary output (default: 4000) |

### Output Format

The directive produces a structured summary with these sections:

```markdown
## Thread Summary

**Directive:** {directive_name}
**Status:** What state the thread was in when it stopped

### Completed Work

- Bullet list of what was accomplished, including key results and data

### Pending Work

- What remained to be done when the thread stopped

### Key Decisions & Context

- Important decisions made during execution
- Relevant data/state that the resumed thread needs

### Tool Results (Key Data)

- Important tool outputs preserved verbatim (IDs, scores, structured data)
```

### What Gets Preserved

The summary prioritizes:

- **Actionable data** — IDs, scores, names, structured results that the resumed thread needs
- **Decision points** — reasoning and choices made during execution
- **Error context** — if the thread errored, what happened and why
- **Pending state** — exactly what remains to be done

The summary discards conversational filler, redundant tool invocations, and intermediate states that are superseded by later results.

### Invocation

Invoke it directly or from a directive's `after_complete` hook:

```python
rye_execute(
    item_type="directive",
    item_id="rye/agent/threads/thread_summary",
    parameters={
        "transcript_content": "<full transcript text>",
        "directive_name": "deployment/deploy_pipeline",
        "max_summary_tokens": 4000
    }
)
```

---

## User-Facing vs. Infrastructure

| Directive                   | Type           | When You Use It                                                           |
| --------------------------- | -------------- | ------------------------------------------------------------------------- |
| `create_directive`          | User-facing    | You want to scaffold a new directive for your project                     |
| `create_tool`               | User-facing    | You want to scaffold a new tool (Python, YAML, Bash, or JS)               |
| `create_knowledge`          | User-facing    | You want to scaffold a new knowledge entry                                |
| `create_threaded_directive` | User-facing    | You want a directive that runs autonomously in a budget-controlled thread |
| `thread_summary`            | Opt-in hook    | Declared in directive `after_complete` hooks for summarization on handoff |

The four creation directives are the primary way to add new items to your project. They handle metadata formatting, file structure, validation, and signing so you don't have to remember the schema manually. Use the [metadata reference knowledge entries](overview.md#knowledge) if you need to write items by hand.
