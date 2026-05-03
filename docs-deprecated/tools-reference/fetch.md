```yaml
id: fetch
title: "rye_fetch"
description: Find items by ID or discover by query
category: tools-reference
tags: [fetch, mcp-tool, api]
version: "2.0.0"
```

# rye_fetch

Two modes — provide `item_id` for ID mode (resolve content), or `query` + `scope` for Query mode (discover items). Cannot use both simultaneously.

## Parameters

| Parameter      | Type   | Mode    | Required      | Default     | Description                                                                         |
| -------------- | ------ | ------- | ------------- | ----------- | ----------------------------------------------------------------------------------- |
| `project_path` | string | Shared  | yes           | —           | Absolute path to the project root                                                   |
| `item_id`      | string | ID      | yes           | —           | Relative path from `.ai/<type>/` without extension (e.g., `"rye/core/create_tool"`) |
| `item_type`    | string | ID      | no            | *(auto)*    | `"directive"`, `"tool"`, or `"knowledge"`. Auto-detects if omitted                  |
| `source`       | string | Both    | no            | —           | **ID mode:** `"project"`, `"user"`, `"system"`, or `"registry"`. Cascades project → user → system when omitted. **Query mode:** `"project"`, `"user"`, `"system"`, `"local"` (all local), `"registry"`, or `"all"` (default) |
| `destination`  | string | ID      | no            | —           | If set and different from `source`, copies the item to this space (`"project"` or `"user"`) |
| `version`      | string | ID      | no            | —           | Version to pull (registry source only)                                              |
| `query`        | string | Query   | yes           | —           | Search query string (supports boolean operators, phrases, wildcards)                |
| `scope`        | string | Query   | yes           | —           | Item type and optional namespace filter in capability format                        |
| `limit`        | int    | Query   | no            | `10`        | Maximum number of results to return                                                 |
| `offset`       | int    | Query   | no            | `0`         | Number of results to skip (for pagination)                                          |
| `sort_by`      | string | Query   | no            | `"score"`   | Sort order: `"score"`, `"date"`, or `"name"`                                        |
| `fields`       | dict   | Query   | no            | `{}`        | Field-specific search queries (e.g., `{"category": "core"}`)                        |
| `filters`      | dict   | Query   | no            | `{}`        | Meta-field filters for exact matching                                               |
| `fuzzy`        | dict   | Query   | no            | `{}`        | Fuzzy matching config: `{"enabled": true, "max_distance": 2}`                       |
| `proximity`    | dict   | Query   | no            | `{}`        | Proximity search config: `{"enabled": true, "max_distance": 5}`                     |

## ID Mode

Resolve item content by ID. Loads the raw content and metadata of a directive, tool, or knowledge item. Optionally copy items between spaces for customization.

### Response

```json
{
  "status": "success",
  "content": "<!-- rye:signed:... -->\n# Create Tool\n...",
  "metadata": {
    "name": "create_tool",
    "path": "/home/user/.ai/directives/rye/core/create_tool.md",
    "extension": ".md",
    "version": "1.0.0"
  },
  "path": "/home/user/.ai/directives/rye/core/create_tool.md",
  "source": "user"
}
```

#### Metadata fields

| Field       | Description                                                |
| ----------- | ---------------------------------------------------------- |
| `name`      | Filename stem (without extension)                          |
| `path`      | Absolute path to the source file                           |
| `extension` | File extension (`.md`, `.py`, `.yaml`, etc.)               |
| `version`   | Extracted from `__version__` or `version="..."` if present |

### Integrity Verification

Items are verified against their signature when loaded. If the content has been modified since signing, the load fails with an integrity error.

### Copying Between Spaces

Set `destination` to copy an item from one space to another. This is useful for pulling system-provided items into your project for customization.

When copying, the file is placed in the destination's type directory, preserving the filename. The response includes both the original path and the copy destination:

```json
{
  "status": "success",
  "content": "...",
  "metadata": { "..." },
  "path": "/path/to/rye/package/.ai/tools/rye/bash.py",
  "source": "system",
  "copied_to": "project",
  "destination_path": "/home/user/my-project/.ai/tools/bash.py"
}
```

#### Valid copy directions

| Source    | Destination | Use case                                    |
| --------- | ----------- | ------------------------------------------- |
| `system`  | `project`   | Customize a built-in item for your project  |
| `system`  | `user`      | Customize a built-in item globally          |
| `user`    | `project`   | Override a user-level item for this project |
| `project` | `user`      | Promote a project item to your global space |

### Error Responses

```json
{
  "status": "error",
  "error": "Item not found: my-project/missing-tool",
  "item_type": "tool",
  "item_id": "my-project/missing-tool"
}
```

## Query Mode

Discover items using keyword search with boolean operators, phrase matching, wildcards, fuzzy matching, and BM25-inspired scoring.

### Scope Format

The `scope` parameter determines which item type to search and optionally restricts to a namespace prefix.

#### Shorthand format

| Scope               | Searches                |
| ------------------- | ----------------------- |
| `"directive"`       | All directives          |
| `"tool"`            | All tools               |
| `"knowledge"`       | All knowledge entries   |
| `"tool.rye.core.*"` | Tools under `rye/core/` |
| `"directive.rye.*"` | Directives under `rye/` |

#### Full capability format

| Scope                              | Searches                    |
| ---------------------------------- | --------------------------- |
| `"rye.fetch.directive.*"`          | All directives              |
| `"rye.fetch.tool.rye.core.*"`      | Tools under `rye/core/`     |
| `"rye.fetch.knowledge.rye.core.*"` | Knowledge under `rye/core/` |

Dots in the namespace portion are converted to path separators. A trailing `.*` or `*` matches all items under that prefix.

### Query Syntax

#### Boolean operators

Queries support `AND`, `OR`, and `NOT` operators. Space-separated terms are implicitly joined with `AND`.

```
file system              → file AND system (implicit)
file AND system          → both terms must match
file OR directory        → either term matches
file NOT temporary       → "file" present, "temporary" absent
(file OR dir) AND read   → grouping with parentheses
```

#### Phrase search

Wrap terms in double quotes to match exact phrases:

```
"file system"            → matches the exact phrase "file system"
"create directive"       → matches "create directive" as a contiguous phrase
```

#### Wildcards

Use `*` to match any sequence of characters within a term:

```
file*                    → matches "file", "filesystem", "file-system"
*search                  → matches "search", "websearch"
*thread*                 → matches anything containing "thread"
```

### Scoring

Results are scored using a BM25-inspired algorithm with field weights:

| Field         | Weight | Description                            |
| ------------- | ------ | -------------------------------------- |
| `title`       | 3.0    | Item title (from metadata/frontmatter) |
| `name`        | 3.0    | Item name (derived from filename)      |
| `description` | 2.0    | Item description                       |
| `category`    | 1.5    | Item category                          |
| `content`     | 1.0    | Body content (preview)                 |

Field weights are loaded from data-driven extractors and can be customized per item type. Scores are normalized to a 0.0–1.0 range.

#### Sort tie-breaking

When multiple results share the same score, ties are broken by:

1. **Source priority:** project (0) → user (1) → system (2)
2. **Item ID:** alphabetical

### Fuzzy Matching

Enable fuzzy matching to find results despite typos or spelling variations:

```python
rye_fetch(
    query="direcive",
    scope="directive",
    project_path="/home/user/my-project",
    fuzzy={"enabled": True, "max_distance": 2}
)
```

The `max_distance` parameter controls the maximum Levenshtein edit distance allowed between query terms and document terms.

### Proximity Search

Find documents where search terms appear near each other:

```python
rye_fetch(
    query="thread orchestrator",
    scope="tool",
    project_path="/home/user/my-project",
    proximity={"enabled": True, "max_distance": 5}
)
```

The `max_distance` parameter sets the maximum number of words allowed between the two terms. Requires at least two query terms.

### Response

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

#### Result fields

| Field         | Description                                               |
| ------------- | --------------------------------------------------------- |
| `id`          | Item ID (relative path without extension)                 |
| `name`        | Item name (filename stem)                                 |
| `description` | Item description from metadata                            |
| `category`    | Item category                                             |
| `score`       | Relevance score (0.0–1.0)                                 |
| `type`        | Item type (`directive`, `tool`, `knowledge`)              |
| `source`      | Space where the item was found                            |
| `preview`     | Truncated content preview                                 |
| `shadows`     | *(optional)* List of spaces this item shadows             |
| `shadowed_by` | *(optional)* Source of the higher-precedence item that shadows this one |

### Shadow Detection

When searching across multiple spaces (`source: "all"`), the same item ID can appear in more than one space. Search results include shadow annotations:

- **`shadows`** — present on the winning item (highest-precedence space). Lists the lower-precedence spaces that have the same item ID.
- **`shadowed_by`** — present on items that are overridden. Contains the source label of the item that takes priority.

```json
{
  "id": "my/custom-tool",
  "source": "project",
  "shadows": [{"space": "system:ryeos-core"}]
}
```

This makes silent shadowing visible — if a project tool overrides a system tool with the same ID, the search results show exactly what happened.

## Examples

### ID Mode — Load a directive from the project space

```python
rye_fetch(
    item_id="rye/core/create_directive",
    project_path="/home/user/my-project",
    source="project"
)
```

### ID Mode — Load a system tool for inspection

```python
rye_fetch(
    item_type="tool",
    item_id="rye/file-system/read",
    project_path="/home/user/my-project",
    source="system"
)
```

### ID Mode — Copy a system directive into the project for customization

```python
rye_fetch(
    item_type="directive",
    item_id="rye/core/create_tool",
    project_path="/home/user/my-project",
    source="system",
    destination="project"
)
```

### ID Mode — Load a knowledge entry

```python
rye_fetch(
    item_type="knowledge",
    item_id="rye/core/tool-metadata-reference",
    project_path="/home/user/my-project",
    source="system"
)
```

### Query Mode — Search all directives

```python
rye_fetch(
    query="create",
    scope="directive",
    project_path="/home/user/my-project"
)
```

### Query Mode — Search tools in a specific namespace

```python
rye_fetch(
    query="read write",
    scope="tool.rye.file-system.*",
    project_path="/home/user/my-project"
)
```

### Query Mode — Search with field-specific queries

```python
rye_fetch(
    query="thread",
    scope="tool",
    project_path="/home/user/my-project",
    fields={"category": "agent"}
)
```

### Query Mode — Search project space only

```python
rye_fetch(
    query="deploy",
    scope="directive",
    project_path="/home/user/my-project",
    source="project"
)
```

### Query Mode — Paginated search

```python
rye_fetch(
    query="*",
    scope="knowledge",
    project_path="/home/user/my-project",
    limit=5,
    offset=10,
    sort_by="name"
)
```

### Query Mode — Boolean query with exclusion

```python
rye_fetch(
    query="file NOT temporary",
    scope="tool",
    project_path="/home/user/my-project"
)
```

### Query Mode — Phrase search

```python
rye_fetch(
    query='"thread directive"',
    scope="tool",
    project_path="/home/user/my-project"
)
```
