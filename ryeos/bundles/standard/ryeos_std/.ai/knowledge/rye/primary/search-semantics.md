<!-- rye:signed:2026-02-26T05:02:40Z:03c6a007153e204591b8c5b23dd1f8516a50d1517af07b4e93a8310cc5f43e07:gVumdzIUT7XhYUiZtjp42u8d79mqKi3E7c4jZ9dRnLcMDKJ1G0IVRsF1OauSF3MacbvQXj3juBXqUZS6kyExCg==:4b987fd4e40303ac -->

```yaml
name: search-semantics
title: "rye_search — MCP Tool Semantics"
entry_type: reference
category: rye/primary
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - search
  - mcp-tool
  - api
references:
  - execute-semantics
  - load-semantics
  - sign-semantics
  - "docs/tools-reference/search.md"
```

# rye_search — MCP Tool Semantics

Find directives, tools, or knowledge entries using keyword search with boolean operators, phrase matching, wildcards, fuzzy matching, and BM25-inspired scoring.

## Parameters

| Parameter      | Type   | Required | Default   | Description                                                           |
| -------------- | ------ | -------- | --------- | --------------------------------------------------------------------- |
| `query`        | string | yes      | —         | Search query (supports boolean operators, phrases, wildcards)         |
| `scope`        | string | yes      | —         | Item type and optional namespace filter in capability format          |
| `project_path` | string | yes      | —         | Absolute path to the project root                                     |
| `space`        | string | no       | `"all"`   | Which spaces to search: `"project"`, `"user"`, `"system"`, or `"all"` |
| `limit`        | int    | no       | `10`      | Maximum results to return                                             |
| `offset`       | int    | no       | `0`       | Results to skip (pagination)                                          |
| `sort_by`      | string | no       | `"score"` | Sort order: `"score"`, `"date"`, or `"name"`                          |
| `fields`       | dict   | no       | `{}`      | Field-specific search queries (e.g., `{"category": "core"}`)          |
| `filters`      | dict   | no       | `{}`      | Meta-field filters for exact matching                                 |
| `fuzzy`        | dict   | no       | `{}`      | Fuzzy config: `{"enabled": true, "max_distance": 2}`                  |
| `proximity`    | dict   | no       | `{}`      | Proximity config: `{"enabled": true, "max_distance": 5}`              |

## Scope Format

Determines item type to search and optionally restricts to a namespace prefix.

**Shorthand:**

| Scope               | Searches                |
| ------------------- | ----------------------- |
| `"directive"`       | All directives          |
| `"tool"`            | All tools               |
| `"knowledge"`       | All knowledge entries   |
| `"tool.rye.core.*"` | Tools under `rye/core/` |
| `"directive.rye.*"` | Directives under `rye/` |

**Full capability format:**

| Scope                               | Searches                    |
| ----------------------------------- | --------------------------- |
| `"rye.search.directive.*"`          | All directives              |
| `"rye.search.tool.rye.core.*"`      | Tools under `rye/core/`     |
| `"rye.search.knowledge.rye.core.*"` | Knowledge under `rye/core/` |

Dots in namespace → path separators. Trailing `.*` or `*` matches all items under that prefix.

## Query Syntax

### Boolean operators

Space-separated terms are implicitly `AND`. Supports `AND`, `OR`, `NOT`, and parentheses:

```
file system              → file AND system (implicit)
file AND system          → both terms must match
file OR directory        → either term matches
file NOT temporary       → "file" present, "temporary" absent
(file OR dir) AND read   → grouping with parentheses
```

### Phrase search

Double quotes for exact phrase matching:

```
"file system"            → matches exact phrase
"create directive"       → matches contiguous phrase
```

### Wildcards

`*` matches any character sequence within a term:

```
file*                    → "file", "filesystem", "file-system"
*search                  → "search", "websearch"
*thread*                 → anything containing "thread"
```

## BM25 Scoring

Results scored with BM25-inspired algorithm using field weights:

| Field         | Weight | Description                            |
| ------------- | ------ | -------------------------------------- |
| `title`       | 3.0    | Item title (from metadata/frontmatter) |
| `name`        | 3.0    | Item name (derived from filename)      |
| `description` | 2.0    | Item description                       |
| `category`    | 1.5    | Item category                          |
| `content`     | 1.0    | Body content (preview)                 |

Scores normalized to 0.0–1.0 range. Field weights are data-driven and customizable per item type.

### Tie-breaking

When scores are equal:
1. **Source priority:** project (0) → user (1) → system (2)
2. **Item ID:** alphabetical

## Fuzzy Matching

Finds results despite typos. `max_distance` = maximum Levenshtein edit distance:

```python
rye_search(
    query="direcive",  # typo
    scope="directive",
    project_path="/home/user/my-project",
    fuzzy={"enabled": True, "max_distance": 2}
)
```

## Proximity Search

Finds documents where terms appear near each other. Requires ≥ 2 query terms:

```python
rye_search(
    query="thread orchestrator",
    scope="tool",
    project_path="/home/user/my-project",
    proximity={"enabled": True, "max_distance": 5}
)
```

## Response Format

```json
{
  "results": [
    {
      "id": "rye/file-system/read",
      "name": "read",
      "description": "Read file contents",
      "category": "file-system",
      "score": 0.8571,
      "type": "tool",
      "source": "system",
      "preview": "Read the contents of a file at the given path..."
    }
  ],
  "total": 3,
  "query": "read file",
  "scope": "tool",
  "space": "all",
  "limit": 10,
  "offset": 0,
  "search_type": "keyword"
}
```

### Result fields

| Field         | Description                                  |
| ------------- | -------------------------------------------- |
| `id`          | Item ID (relative path without extension)    |
| `name`        | Item name (filename stem)                    |
| `description` | Item description from metadata               |
| `category`    | Item category                                |
| `score`       | Relevance score (0.0–1.0)                    |
| `type`        | Item type (`directive`, `tool`, `knowledge`) |
| `source`      | Space where item was found (see Source Detection) |
| `preview`     | Truncated content preview                    |

### Envelope fields

| Field         | Description                                |
| ------------- | ------------------------------------------ |
| `total`       | Total matching results (before pagination) |
| `query`       | Original query string                      |
| `scope`       | Resolved scope                             |
| `space`       | Space filter applied                       |
| `limit`       | Page size                                  |
| `offset`      | Current offset                             |
| `search_type` | Always `"keyword"`                         |

## Source Detection

The `source` label on each result comes from `_detect_source()`. When the search path builder provides a source label (e.g., `"project"`, `"user"`, or `"system:<bundle_id>"`), that label is used directly — it is the authoritative source. Labels starting with `"system"` (including `"system:<bundle_id>"`) are normalized to `"system"`. Path-based detection against known bundle paths and user space is only a fallback for callers that don't provide a label.

## Usage Examples

```python
# Search all directives
rye_search(query="create", scope="directive", project_path="/path")

# Namespace-scoped tool search
rye_search(query="read write", scope="tool.rye.file-system.*", project_path="/path")

# Field-specific query
rye_search(query="thread", scope="tool", project_path="/path", fields={"category": "agent"})

# Project space only
rye_search(query="deploy", scope="directive", project_path="/path", space="project")

# Paginated
rye_search(query="*", scope="knowledge", project_path="/path", limit=5, offset=10, sort_by="name")

# Boolean with exclusion
rye_search(query="file NOT temporary", scope="tool", project_path="/path")

# Phrase search
rye_search(query='"thread directive"', scope="tool", project_path="/path")
```
