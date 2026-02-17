```yaml
id: bundled-knowledge
title: "Bundled Knowledge"
description: Knowledge entries that ship with Rye OS — metadata references for AI agents
category: standard-library
tags: [knowledge, bundled, standard-library, metadata, reference]
version: "1.0.0"
```

# Bundled Knowledge

Rye OS ships three knowledge entries in the system space at `rye/rye/.ai/knowledge/rye/core/`. These are **metadata references** — specifications that AI agents load at runtime to understand how to write valid directives, tools, and knowledge entries.

---

## Bundled Entries

### 1. Directive Metadata Reference

**Item ID:** `rye/core/directive-metadata-reference`

The complete specification for directive metadata. Covers:

- **Root element** — `<directive name="..." version="...">` with required `name` (kebab-case) and `version` (semver) attributes
- **Required metadata** — `<description>`, `<category>`, `<author>`, `<model>`, `<permissions>`, `<cost>`
- **Model tier** — User-defined string (e.g., `fast`, `general`, `orchestrator`, `reasoning`) — not an enum. Supports `fallback` and `id` attributes
- **Permissions** — Hierarchical XML structure: `<permissions>` → `<execute>`/`<search>`/`<load>`/`<sign>` → `<tool>`/`<directive>`/`<knowledge>` → capability pattern. Supports wildcard `*` for broad access
- **Capability strings** — Format: `rye.{primary}.{item_type}.{specifics}` (e.g., `rye.file-system.*`)
- **Cost tracking** — `<cost>` with `<context estimated_usage="..." turns="..." spawn_threshold="...">`, optional `<duration>` and `<spend>`
- **Optional fields** — `<context>` (related files, relationships), `<hooks>` (conditional actions triggered by events)
- **Hooks** — Define `<when>` conditions evaluated against context variables (`cost.current`, `loop_count`, `error.type`, etc.) with `<execute item_type="...">` actions

**Load it:**

```python
rye_load(item_type="knowledge", item_id="rye/core/directive-metadata-reference")
```

### 2. Tool Metadata Reference

**Item ID:** `rye/core/tool-metadata-reference`

The complete specification for tool metadata. Covers:

- **Required fields** — `tool_id` (kebab-case), `category` (matches directory path), `tool_type`, `version` (semver), `description`, `executor_id`
- **Tool types** — `primitive` (atomic, no executor), `script` (executable in a language), `runtime` (language environment), `library` (reusable code)
- **Python tools** — Use dunder variables: `__version__`, `__tool_type__`, `__executor_id__`, `__category__`, `__tool_description__`
- **YAML tools** — Use top-level keys: `tool_id`, `tool_type`, `version`, `executor_id`, etc.
- **Parameters** — List of `{name, type, required, default, description}` with optional constraints (`minimum`, `maximum`, `pattern`, `enum`)
- **Optional fields** — `requires` (capabilities), `parameters`, `outputs`, `actions` (sub-actions), `tags`, `config`, `retry_policy`
- **Entry point** — Python tools implement `execute(params, project_path)` or `main()` as the execution entry point

**Load it:**

```python
rye_load(item_type="knowledge", item_id="rye/core/tool-metadata-reference")
```

### 3. Knowledge Metadata Reference

**Item ID:** `rye/core/knowledge-metadata-reference`

The complete specification for knowledge entry metadata. Covers:

- **Required frontmatter** — `id` (kebab-case), `title`, `category` (matches directory path), `version` (semver), `author`, `created_at` (ISO 8601)
- **File format** — YAML frontmatter between `---` delimiters, followed by markdown content
- **Knowledge graph links** — `references` (outbound links to other entries or URLs), `extends` (inheritance/dependency chain), `used_by` (inbound from directives/tools that apply this knowledge)
- **IDs** — Kebab-case, hierarchical when appropriate (e.g., `patterns/retry-logic`)
- **Categories** — Must match directory structure relative to `knowledge/` parent

**Load it:**

```python
rye_load(item_type="knowledge", item_id="rye/core/knowledge-metadata-reference")
```

---

## Using Knowledge at Runtime

Knowledge entries are designed to be loaded by AI agents during execution. They serve as **documentation for agents** — structured reference material that helps the LLM understand how to produce valid output.

### In Directives

Directives can load knowledge in their process steps to inform the LLM's decisions:

```xml
<step name="load_specs">
  <description>Load the tool metadata spec before creating a new tool</description>
  <action>
    rye_load(item_type="knowledge", item_id="rye/core/tool-metadata-reference")
  </action>
</step>
```

### Via Thread Hooks

Thread hooks can automatically inject knowledge when a thread starts:

```xml
<hooks>
  <hook>
    <when>thread.event == "thread_started"</when>
    <execute item_type="knowledge">rye/core/directive-metadata-reference</execute>
  </hook>
</hooks>
```

This ensures the LLM has the spec loaded before it begins work, without the directive author needing to add explicit load steps.

### Searching Knowledge

All knowledge entries — bundled and user-created — are searchable:

```python
# Search across all knowledge
rye_search(scope="knowledge", query="metadata specification")

# Search with specific source
rye_search(scope="knowledge", query="tool parameters", source="system")
```

---

## Extending the Knowledge Base

The bundled entries cover Rye OS internals. For your own projects, add domain-specific knowledge in `.ai/knowledge/`:

```
.ai/knowledge/
├── domain/
│   ├── api-conventions.md        # Your API patterns
│   └── database-schema.md        # Schema documentation
├── troubleshooting/
│   └── common-deploy-issues.md   # Known issues and fixes
└── patterns/
    └── error-handling.md         # Project error handling patterns
```

Each entry needs the required frontmatter (`id`, `title`, `category`, `version`, `author`, `created_at`) and can use `references`, `extends`, and `used_by` to link into the knowledge graph.

Knowledge entries are searchable immediately after creation:

```python
rye_search(scope="knowledge", query="deployment troubleshooting")
```

And loadable by any directive or thread:

```python
rye_load(item_type="knowledge", item_id="troubleshooting/common-deploy-issues")
```

This makes your project knowledge available to AI agents in the same way the bundled metadata references are — structured, searchable, and injectable at runtime.
