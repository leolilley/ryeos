```yaml
id: search
title: "rye_search"
description: Find directives, tools, or knowledge entries
category: tools-reference
tags: [search, mcp-tool, api]
version: "1.0.0"
```

# rye_search

Find directives, tools, or knowledge entries using keyword search with boolean operators, phrase matching, wildcards, fuzzy matching, and BM25-inspired scoring.

## Parameters

| Parameter      | Type   | Required | Default   | Description                                                           |
| -------------- | ------ | -------- | --------- | --------------------------------------------------------------------- |
| `query`        | string | yes      | —         | Search query string (supports boolean operators, phrases, wildcards)  |
| `scope`        | string | yes      | —         | Item type and optional namespace filter in capability format          |
| `project_path` | string | yes      | —         | Absolute path to the project root                                     |
| `space`        | string | no       | `"all"`   | Which spaces to search: `"project"`, `"user"`, `"system"`, or `"all"` |
| `limit`        | int    | no       | `10`      | Maximum number of results to return                                   |
| `offset`       | int    | no       | `0`       | Number of results to skip (for pagination)                            |
| `sort_by`      | string | no       | `"score"` | Sort order: `"score"`, `"date"`, or `"name"`                          |
| `fields`       | dict   | no       | `{}`      | Field-specific search queries (e.g., `{"category": "core"}`)          |
| `filters`      | dict   | no       | `{}`      | Meta-field filters for exact matching                                 |
| `fuzzy`        | dict   | no       | `{}`      | Fuzzy matching config: `{"enabled": true, "max_distance": 2}`         |
| `proximity`    | dict   | no       | `{}`      | Proximity search config: `{"enabled": true, "max_distance": 5}`       |

## Scope Format

The `scope` parameter determines which item type to search and optionally restricts to a namespace prefix.

### Shorthand format

| Scope               | Searches                |
| ------------------- | ----------------------- |
| `"directive"`       | All directives          |
| `"tool"`            | All tools               |
| `"knowledge"`       | All knowledge entries   |
| `"tool.rye.core.*"` | Tools under `rye/core/` |
| `"directive.rye.*"` | Directives under `rye/` |

### Full capability format

| Scope                               | Searches                    |
| ----------------------------------- | --------------------------- |
| `"rye.search.directive.*"`          | All directives              |
| `"rye.search.tool.rye.core.*"`      | Tools under `rye/core/`     |
| `"rye.search.knowledge.rye.core.*"` | Knowledge under `rye/core/` |

Dots in the namespace portion are converted to path separators. A trailing `.*` or `*` matches all items under that prefix.

## Query Syntax

### Boolean operators

Queries support `AND`, `OR`, and `NOT` operators. Space-separated terms are implicitly joined with `AND`.

```
file system              → file AND system (implicit)
file AND system          → both terms must match
file OR directory        → either term matches
file NOT temporary       → "file" present, "temporary" absent
(file OR dir) AND read   → grouping with parentheses
```

### Phrase search

Wrap terms in double quotes to match exact phrases:

```
"file system"            → matches the exact phrase "file system"
"create directive"       → matches "create directive" as a contiguous phrase
```

### Wildcards

Use `*` to match any sequence of characters within a term:

```
file*                    → matches "file", "filesystem", "file-system"
*search                  → matches "search", "websearch"
*thread*                 → matches anything containing "thread"
```

## Scoring

Results are scored using a BM25-inspired algorithm with field weights:

| Field         | Weight | Description                            |
| ------------- | ------ | -------------------------------------- |
| `title`       | 3.0    | Item title (from metadata/frontmatter) |
| `name`        | 3.0    | Item name (derived from filename)      |
| `description` | 2.0    | Item description                       |
| `category`    | 1.5    | Item category                          |
| `content`     | 1.0    | Body content (preview)                 |

Field weights are loaded from data-driven extractors and can be customized per item type. Scores are normalized to a 0.0–1.0 range.

### Sort tie-breaking

When multiple results share the same score, ties are broken by:

1. **Source priority:** project (0) → user (1) → system (2)
2. **Item ID:** alphabetical

## Fuzzy Matching

Enable fuzzy matching to find results despite typos or spelling variations:

```python
rye_search(
    query="direcive",
    scope="directive",
    project_path="/home/user/my-project",
    fuzzy={"enabled": True, "max_distance": 2}
)
```

The `max_distance` parameter controls the maximum Levenshtein edit distance allowed between query terms and document terms.

## Proximity Search

Find documents where search terms appear near each other:

```python
rye_search(
    query="thread orchestrator",
    scope="tool",
    project_path="/home/user/my-project",
    proximity={"enabled": True, "max_distance": 5}
)
```

The `max_distance` parameter sets the maximum number of words allowed between the two terms. Requires at least two query terms.

## Response

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
| `source`      | Space where the item was found               |
| `preview`     | Truncated content preview                    |

## Examples

### Search all directives

```python
rye_search(
    query="create",
    scope="directive",
    project_path="/home/user/my-project"
)
```

### Search tools in a specific namespace

```python
rye_search(
    query="read write",
    scope="tool.rye.file-system.*",
    project_path="/home/user/my-project"
)
```

### Search with field-specific queries

```python
rye_search(
    query="thread",
    scope="tool",
    project_path="/home/user/my-project",
    fields={"category": "agent"}
)
```

### Search project space only

```python
rye_search(
    query="deploy",
    scope="directive",
    project_path="/home/user/my-project",
    space="project"
)
```

### Paginated search

```python
rye_search(
    query="*",
    scope="knowledge",
    project_path="/home/user/my-project",
    limit=5,
    offset=10,
    sort_by="name"
)
```

### Boolean query with exclusion

```python
rye_search(
    query="file NOT temporary",
    scope="tool",
    project_path="/home/user/my-project"
)
```

### Phrase search

```python
rye_search(
    query='"thread directive"',
    scope="tool",
    project_path="/home/user/my-project"
)
```
