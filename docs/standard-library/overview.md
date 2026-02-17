```yaml
id: standard-library-overview
title: "Standard Library Overview"
description: Everything that ships with Rye OS out of the box
category: standard-library
tags: [standard-library, bundled, system-space, catalog]
version: "1.0.0"
```

# Standard Library Overview

Rye OS ships a **standard library** of directives, tools, and knowledge entries inside the Python package at `rye/rye/.ai/`. These items live in the **system space** — the lowest-priority tier — and are available to every project automatically, without any setup or installation.

When you install Rye OS, every project immediately has access to file-system tools, shell execution, item creation directives, thread orchestration, and more. You never need to copy these files into your project.

## Override Mechanism

System space items can be overridden by placing a file with the same `item_id` in a higher-priority space:

| Space       | Location                        | Priority |
| ----------- | ------------------------------- | -------- |
| **Project** | `.ai/` (project root)           | Highest  |
| **User**    | `~/.ai/` (home directory)       | Middle   |
| **System**  | `rye/rye/.ai/` (Python package) | Lowest   |

Resolution order: **project → user → system**. The first match wins.

For example, to customize the `rye/file-system/read` tool for your project, create `.ai/tools/rye/file-system/read.py` in your project root. Your version will be used instead of the built-in one. The system version remains untouched and continues to serve other projects.

---

## Catalog

### Directives

Five directives ship in `.ai/directives/rye/`:

| Item ID                              | Version | Description                                                                        |
| ------------------------------------ | ------- | ---------------------------------------------------------------------------------- |
| `rye/core/create_directive`          | 3.0.0   | Create a new directive with metadata, validate, and sign                           |
| `rye/core/create_tool`               | 3.0.0   | Create a new tool file with metadata headers and parameter schema, then sign       |
| `rye/core/create_knowledge`          | 3.0.0   | Create a new knowledge entry with YAML frontmatter and sign                        |
| `rye/core/create_threaded_directive` | 2.0.0   | Create a directive with full thread execution support (model, limits, permissions) |
| `rye/agent/threads/thread_summary`   | 1.0.0   | Summarize a thread conversation for context carryover during resumption            |

The first four are **user-facing** creation directives — you invoke them to scaffold new items. `thread_summary` is **infrastructure** — called internally by the thread system during handoff.

See [Bundled Directives](bundled-directives.md) for detailed documentation of each.

### Tools

Tools are organized by namespace under `.ai/tools/rye/`. For detailed documentation of every tool, see the [Tools Reference](tools/index.md).

| Section | Namespace | Tools | Description |
| --- | --- | --- | --- |
| [File System](tools/file-system.md) | `rye/file-system/` | 6 | Read, write, edit (via line IDs), glob, grep, ls |
| [Bash](tools/bash.md) | `rye/bash/` | 1 | Shell command execution |
| [Web](tools/web.md) | `rye/web/` | 2 | Web search and page fetching |
| [MCP Client](tools/mcp.md) | `rye/mcp/` | 3 | Connect to external MCP servers |
| [Primary Tools](tools/primary.md) | `rye/primary/` | 4 | Search, load, execute, sign items |
| [Agent System](tools/agent.md) | `rye/agent/` | 40+ | Thread orchestration, LLM loops, budgets, permissions |
| [Infrastructure](tools/infrastructure.md) | `rye/core/` | 20+ | Parsers, runtimes, extractors, sinks, bundler, registry |

### Knowledge

Three reference entries ship in `.ai/knowledge/rye/`:

| Item ID                                 | Description                                         |
| --------------------------------------- | --------------------------------------------------- |
| `rye/core/directive-metadata-reference` | Complete specification of directive metadata fields |
| `rye/core/tool-metadata-reference`      | Complete specification of tool metadata fields      |
| `rye/core/knowledge-metadata-reference` | Complete specification of knowledge metadata fields |

These are the authoritative references for the metadata schema of each item type. The creation directives consult them when generating new items.

### Other Bundled Files

| Path                             | Description                                          |
| -------------------------------- | ---------------------------------------------------- |
| `bundles/rye-core/manifest.yaml` | Bundle manifest for the core standard library bundle |
| `lockfiles/`                     | Integrity pinning files for signed items             |

---

## What's NOT in the Standard Library

The standard library provides the **infrastructure** — the tools and directives that make Rye OS work. It does not include:

- **Project-specific items** — directives, tools, and knowledge for your particular application (these go in `.ai/`)
- **User customizations** — personal overrides or additions (these go in `~/.ai/`)
- **Registry-downloaded items** — community or team items pulled from the registry via `rye_execute(item_type="tool", item_id="rye/core/registry/registry", ...)`
- **Demo or example content** — the standard library is production infrastructure, not a tutorial

To add items for your project, create files under `.ai/directives/`, `.ai/tools/`, or `.ai/knowledge/` in your project root — or use the bundled creation directives to scaffold them.
