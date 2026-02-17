# RYE OS Knowledge Structure

**Date:** 2026-02-18
**Status:** Planned
**Scope:** All bundled knowledge for rye-os and rye-core packages

---

## Design Principles

1. **Knowledge is agent context, not human docs** — entries are concise reference cards (~200–600 lines) that get injected into LLM prompts. The `docs/` directory is the human-readable documentation. Knowledge distills the rules an agent needs when _doing_ work.
2. **Category mirrors directives and tools exactly** — every directive/tool category (`rye/bash/`, `rye/file-system/`, `rye/mcp/`, etc.) gets a matching knowledge category. An agent working in that domain can load the relevant knowledge.
3. **No duplication with `docs/`** — knowledge entries use `references` frontmatter to point to full docs for deeper context. The body contains only the distilled rules, formats, and constraints.
4. **Cross-reference via `references`/`extends`** — entries form a navigable knowledge graph. Agents can follow links to load related context.
5. **rye-core gets minimal knowledge** — only `rye/core/*` entries (metadata specs, spaces, signing, executor chain). rye-os gets everything.

---

## Current State

3 entries, all in `rye/core/`:

| Current Entry                           | Lines | Purpose                                    |
| --------------------------------------- | ----- | ------------------------------------------ |
| `rye/core/directive-metadata-reference` | 531   | Full spec of directive XML metadata fields |
| `rye/core/tool-metadata-reference`      | 624   | Full spec of tool metadata fields          |
| `rye/core/knowledge-metadata-reference` | 382   | Full spec of knowledge frontmatter fields  |

### Migration: Retire and Replace

All 3 entries will be **deleted** — their content merges into new `rye/authoring/*-format` entries.

| Delete (rye/core/)             | Replace With (rye/authoring/) | Why                                            |
| ------------------------------ | ----------------------------- | ---------------------------------------------- |
| `directive-metadata-reference` | `directive-format`            | Metadata fields are part of the format spec    |
| `tool-metadata-reference`      | `tool-format`                 | Metadata fields are part of the format spec    |
| `knowledge-metadata-reference` | `knowledge-format`            | Frontmatter fields are part of the format spec |

**Rationale:** The metadata references are format specs — they tell an agent what fields to write when creating an item. That's authoring knowledge, not core infrastructure. Having both `*-metadata-reference` in `rye/core/` and `*-format` in `rye/authoring/` would be confusing duplication. The authoring entries become the single canonical source: file structure + all metadata fields + body conventions + validation rules + examples.

**Migration steps:**

1. Create the 3 `rye/authoring/*-format` entries, incorporating metadata field specs from the old entries
2. Delete the 3 `rye/core/*-metadata-reference` files
3. Update any `references` in other knowledge entries that pointed to the old IDs

---

## Bundle Boundaries

### rye-core bundle (`pip install rye-core`)

Ships only `rye/core/*` knowledge:

- Terminology, spaces, signing, executor chain, parsers, capability strings, etc.

### rye-os bundle (`pip install rye-os`)

Ships all `rye/*` knowledge. Includes everything in rye-core plus agent, authoring, registry, primary, integrations.

---

## Full Knowledge Map

### `rye/core/` — Foundations (rye-core + rye-os)

These are the concepts every agent needs regardless of what it's doing.

```
rye/core/
├── terminology.md                    NEW — naming conventions, item types,
│                                       case rules (kebab vs snake), project
│                                       vocabulary
├── ai-directory.md                   NEW — .ai/ structure, what goes where,
│                                       outputs/ vs items, file extensions
│                                       by type
├── three-tier-spaces.md              NEW — project→user→system resolution
│                                       order, paths, override semantics,
│                                       USER_SPACE env var
├── input-interpolation.md            NEW — {input:name}, {input:name?},
│                                       {input:name:default}, where
│                                       interpolation runs (body, actions,
│                                       content, raw)
├── capability-strings.md             NEW — rye.{action}.{type}.{id} format,
│                                       fnmatch wildcards, permission
│                                       hierarchy, god mode, principle
│                                       of least privilege
├── signing-and-integrity.md          NEW — content hashing, Ed25519, lockfiles,
│                                       signature comment format, when
│                                       re-signing is required
├── executor-chain.md                 NEW — tool→runtime→primitive chain,
│                                       chain resolution, space compatibility
│                                       rules, ENV_CONFIG resolution
└── parsers.md                        NEW — markdown_xml, markdown_frontmatter,
                                        python_ast, yaml — what each parser
                                        extracts, when each is used
```

**8 entries (all new — old \*-metadata-reference entries migrate to rye/authoring/)**

### `rye/core/bundler/` — Bundle system

```
rye/core/bundler/
└── bundle-format.md                  NEW — manifest structure, entry points,
                                        pyproject.toml registration,
                                        bundle verification, category scoping
```

**1 entry**

### `rye/core/registry/` — Registry concepts

```
rye/core/registry/
├── registry-api.md                   NEW — endpoints, auth flow (device code),
│                                       namespaces, push/pull/search semantics,
│                                       version resolution
└── trust-model.md                    NEW — key pinning, registry key bootstrap,
                                        trust-on-first-use, signature
                                        verification on pull
```

**2 entries**

### `rye/primary/` — The 4 MCP tools

Agents need to know the exact semantics of the 4 MCP tools — parameters, return shapes, error conditions.

```
rye/primary/
├── execute-semantics.md              NEW — routing by item_type, directive
│                                       parsing + interpolation, tool chain
│                                       execution, knowledge loading, dry_run
│                                       behavior, response shapes per type
├── search-semantics.md               NEW — BM25 scoring, fuzzy matching,
│                                       space filtering, scope parameter,
│                                       result format, limit behavior
├── load-semantics.md                 NEW — raw content return, source/
│                                       destination for copying, sections
│                                       parameter, load vs execute distinction
└── sign-semantics.md                 NEW — hash computation, signature format,
                                        re-signing rules, verification flow,
                                        IntegrityError conditions
```

**4 entries**

### `rye/agent/` — Threading and orchestration

The most complex domain — agents building multi-thread pipelines need dense reference material.

```
rye/agent/
├── provider-configuration.md         NEW — provider YAML format, model tiers
│                                       (low/haiku/sonnet/orchestrator),
│                                       tier→model mapping, API key setup,
│                                       fallback behavior
└── threads/
    ├── thread-lifecycle.md           NEW — creation→boot→loop→finalize,
    │                                   limit resolution (defaults→directive
    │                                   →overrides→parent caps), depth
    │                                   decrement, spawn counting
    ├── prompt-rendering.md           NEW — _render_prompt structure:
    │                                   DIRECTIVE_INSTRUCTION → name/desc →
    │                                   preamble → body → <returns>,
    │                                   <outputs>→<returns> transformation,
    │                                   what's excluded from prompt
    ├── spawning-patterns.md          NEW — sync vs async, thread chains
    │                                   (continuation), budget reservation,
    │                                   wait_threads, get_result, collect
    │                                   patterns, fan-out/fan-in
    ├── limits-and-safety.md          NEW — SafetyHarness loop, limit types
    │                                   (turns, tokens, spend, depth, spawns),
    │                                   hook system (thread_started, error,
    │                                   limit, after_step), control actions
    │                                   (continue, retry, terminate)
    ├── permissions-in-threads.md     NEW — capability token generation,
    │                                   fail-closed enforcement, tool
    │                                   filtering, permission inheritance
    │                                   from directive to thread
    ├── orchestrator-patterns.md      NEW — orchestrator vs leaf vs
    │                                   coordinator roles, result collection,
    │                                   parallel fan-out, sequential phases,
    │                                   error handling in orchestrators
    └── persistence-and-state.md      NEW — thread registry, state store,
                                        artifact store, budget persistence,
                                        transcript, continuation state
```

**8 entries**

### `rye/authoring/` — Writing items (absorbs old metadata references)

Agents that create new directives/tools/knowledge need format rules. These entries are the canonical format specs — they absorb the full metadata field specifications from the retired `rye/core/*-metadata-reference` entries plus file structure, body conventions, and validation rules.

```
rye/authoring/
├── directive-format.md               NEW — absorbs rye/core/directive-metadata-reference
│                                       Two-zone rule (XML fence = infrastructure,
│                                       process steps = LLM), anatomy, signature
│                                       line, ALL metadata fields (model, limits,
│                                       permissions, cost, hooks, context),
│                                       inputs/outputs, success_criteria,
│                                       validation rules, best practices
├── tool-format.md                    NEW — absorbs rye/core/tool-metadata-reference
│                                       Entry point function signature,
│                                       __executor_id__, CONFIG_SCHEMA,
│                                       __metadata__, ALL metadata fields,
│                                       return dict format, supported
│                                       languages/runtimes, validation rules
└── knowledge-format.md               NEW — absorbs rye/core/knowledge-metadata-reference
                                        ALL frontmatter fields, entry_type
                                        (reference/learning/pattern), body
                                        conventions, knowledge graph via
                                        references/extends/used_by, size
                                        guidelines, validation rules
```

**3 entries (replace 3 retired entries from rye/core/)**

### `rye/bash/` — Shell execution

```
rye/bash/
└── bash-execution.md                 NEW — command execution via subprocess,
                                        parameter format, timeout behavior,
                                        working directory, environment
                                        variables, exit codes, security
                                        constraints
```

**1 entry**

### `rye/file-system/` — File operations

```
rye/file-system/
└── file-operations.md                NEW — read/write/edit_lines/ls/glob/grep
                                        tool parameters and behaviors,
                                        path resolution (relative to
                                        project_path), edit_lines line-ID
                                        caching, glob patterns, grep
                                        regex syntax, write modes
```

**1 entry**

### `rye/lsp/` — Language server

```
rye/lsp/
└── lsp-integration.md                NEW — LSP action types (diagnostics,
                                        hover, definition, references,
                                        completions), language support,
                                        server lifecycle, response
                                        formats
```

**1 entry**

### `rye/mcp/` — External MCP integration

```
rye/mcp/
└── mcp-integration.md                NEW — add/discover/connect/refresh/
                                        remove server lifecycle, transport
                                        types (stdio/http), tool discovery,
                                        server config format, tool call
                                        parameters
```

**1 entry**

### `rye/web/` — Web tools

```
rye/web/
└── web-tools.md                      NEW — websearch params and response
                                        format, webfetch modes (markdown/
                                        text/html), practical usage
                                        patterns
```

**1 entry**

---

## Summary

| Category             | Create | Delete | Net    | Bundle            |
| -------------------- | ------ | ------ | ------ | ----------------- |
| `rye/core/`          | 8      | 3      | 8      | rye-core + rye-os |
| `rye/core/bundler/`  | 1      | 0      | 1      | rye-core + rye-os |
| `rye/core/registry/` | 2      | 0      | 2      | rye-core + rye-os |
| `rye/primary/`       | 4      | 0      | 4      | rye-os            |
| `rye/agent/`         | 1      | 0      | 1      | rye-os            |
| `rye/agent/threads/` | 7      | 0      | 7      | rye-os            |
| `rye/authoring/`     | 3      | 0      | 3      | rye-os            |
| `rye/bash/`          | 1      | 0      | 1      | rye-os            |
| `rye/file-system/`   | 1      | 0      | 1      | rye-os            |
| `rye/lsp/`           | 1      | 0      | 1      | rye-os            |
| `rye/mcp/`           | 1      | 0      | 1      | rye-os            |
| `rye/web/`           | 1      | 0      | 1      | rye-os            |
| **Total**            | **31** | **3**  | **31** |                   |

rye-core bundle: 11 entries (rye/core/\* tree)
rye-os bundle: 31 entries (all)

### Category Parity Check

Every directive/tool category gets a knowledge category:

| Category           | Directives | Tools | Knowledge |
| ------------------ | ---------- | ----- | --------- |
| `rye/core/`        | ✅         | ✅    | ✅ (8)    |
| `rye/primary/`     | ✅         | ✅    | ✅ (4)    |
| `rye/agent/`       | ✅         | ✅    | ✅ (8)    |
| `rye/authoring/`   | ✅         | —     | ✅ (3)    |
| `rye/bash/`        | ✅         | ✅    | ✅ (1)    |
| `rye/file-system/` | ✅         | ✅    | ✅ (1)    |
| `rye/lsp/`         | ✅         | ✅    | ✅ (1)    |
| `rye/mcp/`         | ✅         | ✅    | ✅ (1)    |
| `rye/web/`         | ✅         | ✅    | ✅ (1)    |

---

## Entry Style Guide

Conventions for all knowledge entries:

1. **Signature comment** on line 1 (added by `rye_sign`)
2. **YAML frontmatter**: `id`, `title`, `entry_type`, `category`, `version`, `author`, `created_at`, `tags`, `references`
3. **Body**: markdown with tables, code blocks, examples
4. **Size**: 200–600 lines typical. Metadata specs can go longer. Operational guides stay shorter.
5. **`references`** links to sibling knowledge AND to `docs/` paths for full human docs
6. **`entry_type`**: `reference` for specs/formats, `pattern` for usage patterns, `learning` for operational insights
7. **No tutorial prose** — tables, rules, formats, constraints. An agent reads this to know _what to produce_, not to learn the concept.

### Entry template

```markdown
---
id: entry-name
title: Entry Title
entry_type: reference
category: rye/agent/threads
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - tag1
  - tag2
  - tag3
references:
  - sibling-entry-id
  - "docs/path/to/full-doc.md"
---

# Entry Title

One-line summary of what this covers.

## Section

Rules, tables, code blocks. Dense and scannable.
```

---

## Cross-Reference Map

How knowledge entries relate to each other and to docs:

```
                    ┌─────────────────────────┐
                    │  rye/core/terminology    │
                    │  (naming, vocabulary)    │
                    └────────┬────────────────┘
                             │ extends
            ┌────────────────┼────────────────┐
            ▼                ▼                ▼
   ┌────────────────┐ ┌──────────────┐ ┌─────────────────┐
   │ rye/authoring/  │ │ rye/authoring│ │ rye/authoring/  │
   │ directive-      │ │ /tool-       │ │ knowledge-      │
   │ format          │ │ format       │ │ format          │
   └───────┬─────────┘ └──────┬──────┘ └────────┬────────┘
           │                  │                  │
           │ references       │ references       │ references
           ▼                  ▼                  ▼
   ┌────────────────┐ ┌──────────────┐ ┌─────────────────┐
   │ rye/core/      │ │ rye/core/    │ │ rye/core/       │
   │ input-         │ │ executor-    │ │ three-tier-     │
   │ interpolation  │ │ chain        │ │ spaces          │
   └───────┬─────────┘ └──────┬──────┘ └─────────────────┘
           │                  │
           │ used_by          │ used_by
           ▼                  ▼
   ┌────────────────┐ ┌──────────────┐
   │ rye/agent/     │ │ rye/primary/ │
   │ threads/       │ │ execute-     │
   │ prompt-        │ │ semantics    │
   │ rendering      │ │              │
   └────────────────┘ └──────────────┘

   ┌────────────────┐    ┌───────────────────┐
   │ rye/core/      │◄───│ rye/agent/threads/ │
   │ capability-    │    │ permissions-in-    │
   │ strings        │    │ threads            │
   └────────────────┘    └───────────────────┘

   ┌────────────────┐    ┌───────────────────┐
   │ rye/core/      │◄───│ rye/core/registry/ │
   │ signing-and-   │    │ trust-model        │
   │ integrity      │    │                    │
   └────────────────┘    └───────────────────┘
```

---

## Priority Order

Create in this order (each group builds on the previous):

**Phase 1: Foundations** (rye-core, needed by everything else)

1. `rye/core/terminology`
2. `rye/core/ai-directory`
3. `rye/core/three-tier-spaces`
4. `rye/core/input-interpolation`
5. `rye/core/capability-strings`
6. `rye/core/signing-and-integrity`
7. `rye/core/executor-chain`
8. `rye/core/parsers`

**Phase 2: MCP tool semantics** (how agents call rye) 9. `rye/primary/execute-semantics` 10. `rye/primary/search-semantics` 11. `rye/primary/load-semantics` 12. `rye/primary/sign-semantics`

**Phase 3: Authoring** (how agents create items — replaces old \*-metadata-reference entries) 13. `rye/authoring/directive-format` ← absorbs `rye/core/directive-metadata-reference` 14. `rye/authoring/tool-format` ← absorbs `rye/core/tool-metadata-reference` 15. `rye/authoring/knowledge-format` ← absorbs `rye/core/knowledge-metadata-reference`

**Phase 4: Threading** (orchestration domain) 16. `rye/agent/provider-configuration` 17. `rye/agent/threads/thread-lifecycle` 18. `rye/agent/threads/prompt-rendering` 19. `rye/agent/threads/limits-and-safety` 20. `rye/agent/threads/permissions-in-threads` 21. `rye/agent/threads/spawning-patterns` 22. `rye/agent/threads/orchestrator-patterns` 23. `rye/agent/threads/persistence-and-state`

**Phase 5: Distribution** (bundles and registry) 24. `rye/core/bundler/bundle-format` 25. `rye/core/registry/registry-api` 26. `rye/core/registry/trust-model`

**Phase 6: Tool categories** (one knowledge entry per tool category) 27. `rye/bash/bash-execution` 28. `rye/file-system/file-operations` 29. `rye/lsp/lsp-integration` 30. `rye/mcp/mcp-integration` 31. `rye/web/web-tools`

---

## Docs → Knowledge Mapping

Each knowledge entry distills from one or more docs. Not all docs become knowledge — some are tutorials or overviews that don't produce actionable agent context.

| Knowledge Entry          | Distills From (docs/)                                                        |
| ------------------------ | ---------------------------------------------------------------------------- |
| `terminology`            | — (new, no existing doc)                                                     |
| `ai-directory`           | `getting-started/ai-directory.md`                                            |
| `three-tier-spaces`      | `internals/three-tier-spaces.md`                                             |
| `input-interpolation`    | `authoring/directives.md` (section), `tools-reference/execute.md` (section)  |
| `capability-strings`     | `orchestration/permissions-and-capabilities.md`                              |
| `signing-and-integrity`  | `internals/integrity-and-signing.md`                                         |
| `executor-chain`         | `internals/executor-chain.md`                                                |
| `parsers`                | `standard-library/tools/core.md`, `standard-library/tools/infrastructure.md` |
| `execute-semantics`      | `tools-reference/execute.md`                                                 |
| `search-semantics`       | `tools-reference/search.md`                                                  |
| `load-semantics`         | `tools-reference/load.md`                                                    |
| `sign-semantics`         | `tools-reference/sign.md`                                                    |
| `directive-format`       | `authoring/directives.md` + old `directive-metadata-reference` content       |
| `tool-format`            | `authoring/tools.md` + old `tool-metadata-reference` content                 |
| `knowledge-format`       | `authoring/knowledge.md` + old `knowledge-metadata-reference` content        |
| `provider-configuration` | `orchestration/overview.md` (section)                                        |
| `thread-lifecycle`       | `orchestration/thread-lifecycle.md`                                          |
| `prompt-rendering`       | `authoring/directives.md` (section), source code                             |
| `limits-and-safety`      | `orchestration/safety-and-limits.md`                                         |
| `permissions-in-threads` | `orchestration/permissions-and-capabilities.md`                              |
| `spawning-patterns`      | `orchestration/spawning-children.md`                                         |
| `orchestrator-patterns`  | `orchestration/building-a-pipeline.md`                                       |
| `persistence-and-state`  | `orchestration/continuation-and-resumption.md`                               |
| `bundle-format`          | `internals/packages-and-bundles.md`                                          |
| `registry-api`           | `registry/sharing-items.md`, `registry/agent-integration.md`                 |
| `trust-model`            | `registry/trust-model.md`                                                    |
| `bash-execution`         | `standard-library/tools/bash.md`                                             |
| `file-operations`        | `standard-library/tools/file-system.md`                                      |
| `lsp-integration`        | `standard-library/tools/index.md` (lsp section)                              |
| `mcp-integration`        | `standard-library/tools/mcp.md`                                              |
| `web-tools`              | `standard-library/tools/web.md`                                              |

Docs with NO knowledge counterpart (tutorials/overviews — agent doesn't need these as context):

- `getting-started/installation.md` — human setup instructions
- `getting-started/quickstart.md` — tutorial walkthrough
- `orchestration/overview.md` — conceptual intro (covered by specific entries)
- `standard-library/overview.md` — catalog listing
- `standard-library/bundled-*.md` — catalog listings
- `internals/architecture.md` — high-level overview
- `internals/lilux-primitives.md` — too low-level for agent context
