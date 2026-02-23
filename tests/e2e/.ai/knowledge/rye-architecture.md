---
name: rye-architecture
title: Rye Architecture Overview
version: "1.0.0"
entry_type: reference
category: "architecture"
tags: [rye, architecture, data-driven, threads, directives]
created_at: "2026-02-09"
---

# Rye Architecture Overview

Rye is a data-driven agent execution system built on three primitives.

## Core Primitives

1. **Directives** — Declarative task specifications in markdown+XML. They declare metadata (model, limits, permissions, hooks) and the agent follows the steps.
2. **Tools** — Executable Python or YAML files with `CONFIG_SCHEMA` and `__executor_id__`. Resolved via three-tier space precedence (project > user > system).
3. **Knowledge** — Markdown files with YAML frontmatter. Domain information, patterns, and learnings that agents pull into context.

## Execution Flow

- A directive is parsed from `.ai/directives/`
- `thread_directive.py` spawns a thread with a SafetyHarness
- Permissions are mapped to tools via `PERMISSION_TO_TOOLS`
- Provider YAML defines the LLM API shape (data-driven, not hardcoded)
- The multi-turn tool-use loop runs until the LLM stops calling tools
- Hooks fire on events like `after_complete` or `error`

## Design Principles

- **Everything is data-driven**: No hardcoded provider formats, tool names, or directive names
- **Capability tokens**: Permission enforcement via Ed25519-signed tokens
- **Three-tier resolution**: project > user > system for all item types
- **Safety harness**: Cost tracking, limit enforcement, hook evaluation wraps every thread

## Key Directories

- `.ai/directives/` — Task specifications
- `.ai/tools/` — Executable tools with CONFIG_SCHEMA
- `.ai/knowledge/` — Domain knowledge entries
- `.ai/tools/agent/providers/` — LLM provider configurations
