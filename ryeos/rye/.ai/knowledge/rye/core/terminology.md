<!-- rye:signed:2026-02-23T00:43:10Z:b0e8511e1f87873b84c4fcbdb26de2a65e04e4e1f15bcb701a2c6917b509a70d:LRtMBNKL_ghmYPchLYfQ-gnryd-qa1VNkSvkWE2T2DwdpPDFtiuTiInof6euSB6A16LLHki3fJaFJOdhFG5nAg==:9fbfabe975fa5a7f -->

```yaml
id: terminology
title: Rye OS Terminology & Naming Conventions
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - terminology
  - naming
  - conventions
  - item-types
  - directive
  - tool
  - knowledge
  - spaces
  - project
  - user
  - system
  - item-id
  - namespace
references:
  - "docs/getting-started/quickstart.md"
```

# Rye OS Terminology & Naming Conventions

Canonical vocabulary and naming rules for the Rye OS project.

## Project Names

| Term       | Usage                                                                 |
| ---------- | --------------------------------------------------------------------- |
| **Rye OS** | The full product name (two words, capital R, capital OS)               |
| **RYE**    | Acronym form — used in env vars (`RYE_PYTHON`, `RYE_PARENT_THREAD_ID`) |
| **rye**    | Lowercase — Python package name (`import rye`), CLI, pip install name |
| **rye-os** | Hyphenated — GitHub repo name, PyPI package, bundle IDs               |
| **rye-core** | The core Python package containing the standard library             |
| **rye-mcp** | The MCP server package                                               |
| **Lilux**  | The low-level primitives layer (subprocess, HTTP, integrity, signing) |

## The Three Item Types

| Item Type     | What It Is                                  | File Format                    | Storage Directory     |
| ------------- | ------------------------------------------- | ------------------------------ | --------------------- |
| **directive** | Multi-step workflow definition for agents   | `.md` (Markdown + embedded XML) | `.ai/directives/`     |
| **tool**      | Executable script or config                 | `.py`, `.yaml`, `.sh`, `.js`   | `.ai/tools/`          |
| **knowledge** | Domain information, patterns, learnings     | `.md` (Markdown + YAML frontmatter) | `.ai/knowledge/`  |

## ID Conventions

### Item IDs

The item ID is the **relative path** from `.ai/<type>/` to the file, **without the file extension**.

```
.ai/directives/rye/core/create_directive.md  →  item_id = "rye/core/create_directive"
.ai/tools/rye/bash/bash.py                   →  item_id = "rye/bash/bash"
.ai/knowledge/project_conventions.md         →  item_id = "project_conventions"
```

### Case Rules

| Context                | Convention     | Example                          |
| ---------------------- | -------------- | -------------------------------- |
| Directive file names   | `snake_case`   | `create_directive.md`            |
| Tool file names        | `snake_case`   | `bash.py`, `python/script.yaml` |
| Knowledge file names   | `kebab-case`   | `directive-metadata-reference.md` |
| Knowledge IDs          | `kebab-case`   | `directive-metadata-reference`   |
| Directive names (XML)  | `snake_case`   | `<directive name="create_directive">` |
| Namespace directories  | `kebab-case`   | `rye/file-system/`, `rye/core/` |
| Bundle IDs             | `kebab-case`   | `rye-core`, `rye-os`            |
| Env vars               | `UPPER_SNAKE`  | `USER_SPACE`, `RYE_PYTHON`      |
| Capability strings     | `dot.separated`| `rye.execute.tool.rye.bash.bash` |
| Python packages        | `snake_case`   | `rye_core`, `rye_mcp`           |

### Namespace Convention

Items are organized into namespaces using directory nesting. The first path segment identifies the owner.

```
rye/core/     — ships with rye-os (standard library)
rye/bash/     — built-in bash tools
rye/web/      — built-in web tools
rye/file-system/ — built-in file system tools
rye/agent/    — thread orchestration system
<project>/    — project-specific items
```

## MCP Tool Names

The four MCP-exposed tools use the `rye_` prefix:

| MCP Tool      | Purpose                           |
| ------------- | --------------------------------- |
| `rye_execute` | Execute directives, tools, knowledge |
| `rye_search`  | Search across items               |
| `rye_load`    | Load/inspect/copy items           |
| `rye_sign`    | Validate and sign items           |

## Key Concepts

| Term               | Definition                                                        |
| ------------------ | ----------------------------------------------------------------- |
| **space**          | One of three resolution tiers: project, user, system              |
| **chain**          | The tool → runtime → primitive execution path                     |
| **primitive**      | Terminal chain node — maps to a Lilux class (subprocess, HTTP)    |
| **runtime**        | YAML config defining how a tool type is executed                  |
| **bundle**         | Packaged collection of items distributed as a Python package      |
| **integrity**      | SHA-256 content hash + Ed25519 signature verification             |
| **lockfile**       | Pinned chain with integrity hashes for reproducible execution     |
| **capability**     | Permission token controlling what a thread can do                 |
| **thread**         | Isolated LLM execution context with limits and permissions        |
| **anchor**         | Module resolution root for multi-file tool dependencies           |
| **extractor**      | YAML config defining how metadata is indexed per item type        |
| **sink**           | Output destination for thread events (file, websocket, null)      |
