```yaml
id: index
title: "Rye OS Documentation"
description: Documentation for Rye OS — the MCP server that gives AI agents a portable .ai/ directory system
category: root
tags: [index, documentation, ryeos]
version: "1.0.0"
```

# Rye OS Documentation

Rye OS is an MCP server that gives AI agents a portable `.ai/` directory system. Agents interact with it through 4 MCP tools to search, load, execute, and sign three item types: **directives** (workflow instructions), **tools** (executable scripts), and **knowledge** (domain information).

---

## Getting Started

- [Installation](getting-started/installation.md) — Install and configure Rye OS as an MCP server
- [Quickstart](getting-started/quickstart.md) — Create your first directive, tool, and knowledge entry
- [The .ai/ Directory](getting-started/ai-directory.md) — Structure, conventions, and the 3-tier space system

## MCP Tools Reference

The 4 tools that agents use to interact with Rye OS:

- [rye_execute](tools-reference/execute.md) — Execute directives, tools, or knowledge items
- [rye_load](tools-reference/load.md) — Load item content for inspection or copy between spaces
- [rye_search](tools-reference/search.md) — Find items with keyword search, fuzzy matching, and BM25 scoring
- [rye_sign](tools-reference/sign.md) — Validate and sign items with integrity hashes

## Authoring Items

- [Directives](authoring/directives.md) — Write workflow instructions with XML metadata and process steps
- [Tools](authoring/tools.md) — Write executable tools in Python, YAML, Bash, or JavaScript
- [Custom Runtimes](authoring/custom-runtimes.md) — Add support for new languages by creating runtime YAML configurations
- [Knowledge](authoring/knowledge.md) — Write domain information with YAML frontmatter

## Orchestration

Thread-based orchestration is the flagship capability — AI agents coordinating multi-step workflows through hierarchical thread trees.

- [Overview](orchestration/overview.md) — The orchestration pattern and why it works
- [Thread Lifecycle](orchestration/thread-lifecycle.md) — Creation, execution, and finalization
- [Spawning Children](orchestration/spawning-children.md) — Spawn, coordinate, and collect results
- [Safety and Limits](orchestration/safety-and-limits.md) — Cost controls, turn limits, and the SafetyHarness
- [Permissions and Capabilities](orchestration/permissions-and-capabilities.md) — Capability tokens and fail-closed security
- [Continuation and Resumption](orchestration/continuation-and-resumption.md) — Context limit handoffs and user-driven resumption
- [State Graphs](orchestration/state-graphs.md) — Declarative, code-free workflows as YAML graph tools
- [Streaming](orchestration/streaming.md) — Real-time token streaming and transcript rendering
- [Building a Pipeline](orchestration/building-a-pipeline.md) — Step-by-step tutorial with a real-world example

## Standard Library

Everything that ships with Rye OS — the standard bundle, plus optional web and code bundles:

- [Overview](standard-library/overview.md) — Full catalog of bundled items
- [Bundled Directives](standard-library/bundled-directives.md) — Item creation and thread management directives
- [Bundled Tools](standard-library/bundled-tools.md) — File system, bash, web, MCP, orchestration engine, and more
- [Bundled Knowledge](standard-library/bundled-knowledge.md) — Metadata references for AI agents

## Internals

For contributors and those who want to understand how Rye OS works under the hood:

- [Architecture](internals/architecture.md) — Layers, components, and data flow
- [Packages and Bundles](internals/packages-and-bundles.md) — pip packages, bundle entry points, and dependency layering
- [Executor Chain](internals/executor-chain.md) — Multi-layer tool resolution and execution
- [Runtimes](internals/runtimes.md) — How runtimes configure tool execution, interpreter resolution, and the 7 standard runtimes
- [Three-Tier Spaces](internals/three-tier-spaces.md) — Project, user, and system space resolution
- [Integrity and Signing](internals/integrity-and-signing.md) — Content hashing, Ed25519, and lockfiles
- [Lillux Primitives](internals/lillux-primitives.md) — The microkernel layer

## Registry

- [Sharing Items](registry/sharing-items.md) — Push, pull, and discover items through the registry
