# Search Tool (`mcp__rye__search`)

## Purpose

Search for items across directives, tools, or knowledge entries using **keyword-based search** with flexible query patterns. Designed for natural language queries from LLMs while keeping implementation simple and dependency-free.

**Design Philosophy:** Grep-like power with LLM-friendly interface. No external dependencies, works offline.

---

## Request Schema

```json
{
  "item_type": "directive" | "tool" | "knowledge",  // Required
  "query": "string",                                 // Required: Search query
  "source": "project" | "user" | "all",            // Default: "project"
  "limit": 10,                                       // Default: 10
  "offset": 0,                                       // Default: 0
  "sort_by": "score" | "date" | "name",             // Default: "score"
  "project_path": "/path/to/project",                  // Required
  "fields": {...},                                    // Optional: Field-specific search
  "filters": {...},                                   // Optional: Meta-field filters
  "fuzzy": {...},                                    // Optional: Fuzzy matching
  "proximity": {...}                                  // Optional: Proximity search
}
```

---

## Response Schema

```json
{
  "results": [
    {
      "id": "string",
      "type": "directive" | "tool" | "knowledge",
      "score": 0.95,
      "preview": "string",
      "metadata": {
        "version": "1.0.0",
        "category": "automation",
        "description": "...",
        "tags": ["deprecated", "beta"],
        "created_at": "2024-01-15T10:30:00Z",
        "updated_at": "2024-02-01T14:20:00Z"
      },
      "source": "project" | "user",
      "path": "/path/to/item.md"
    }
  ],
  "total": 5,
  "query": "string",
  "item_type": "tool",
  "source": "project",
  "limit": 10,
  "offset": 0,
  "search_type": "keyword"
}
```

---

## Query Syntax

### 1. Simple Keywords

Search for individual words (space-separated AND):

```json
{
  "item_type": "tool",
  "query": "scrape http",
  "project_path": "/home/user/myproject"
}
```

**Matches:** Items containing "scrape" **AND** "http"

---

### 2. Phrase Search

Use quotes for exact phrases:

```json
{
  "item_type": "knowledge",
  "query": "\"lead generation\"",
  "project_path": "/home/user/myproject"
}
```

**Matches:** Items with exact phrase "lead generation"

---

### 3. Wildcards

Use `*` for partial matches:

```json
{
  "item_type": "tool",
  "query": "scrap*",
  "project_path": "/home/user/myproject"
}
```

**Matches:** `scrape`, `scraper`, `scraping`, `scrapers`

```json
{
  "item_type": "tool",
  "query": "*scraper*",
  "project_path": "/home/user/myproject"
}
```

**Matches:** `http-scraper`, `scraper`, `scraper-v2`

---

### 4. Boolean Operators

Combine terms with logic:

```json
{
  "item_type": "tool",
  "query": "scrape AND (http OR api) NOT test",
  "project_path": "/home/user/myproject"
}
```

| Operator | Meaning              | Example                    |
| -------- | -------------------- | -------------------------- |
| `AND`    | All terms must match | `scrape AND http`          |
| `OR`     | Any term can match   | `http OR api`              |
| `NOT`    | Exclude term         | `test NOT debug`           |
| `()`     | Group terms          | `(http OR api) AND scrape` |

**Default behavior:** Words separated by spaces = AND

---

### 5. Field-Specific Search

Target specific fields:

```json
{
  "item_type": "tool",
  "fields": {
    "title": "scraper",
    "description": "http retry",
    "category": "automation",
    "content": "timeout"
  },
  "project_path": "/home/user/myproject"
}
```

**Combine with query:**

```json
{
  "item_type": "tool",
  "query": "http",
  "fields": {
    "title": "scraper"
  },
  "project_path": "/home/user/myproject"
}
```

**Matches:** Items with "scraper" in title AND "http" anywhere

**Available fields:**

| Field         | Type   | Description         | Weight |
| ------------- | ------ | ------------------- | ------ |
| `title`       | string | Item title/name     | 3.0    |
| `name`        | string | Tool/directive name | 3.0    |
| `description` | string | Summary text        | 2.0    |
| `category`    | string | Category identifier | 1.5    |
| `content`     | string | Full item content   | 1.0    |

---

### 6. Meta-Field Filters

Filter by metadata fields:

```json
{
  "item_type": "tool",
  "query": "scraper",
  "filters": {
    "category": "automation",
    "version": ">=1.0.0",
    "tags": ["deprecated"],
    "date_from": "2024-01-01",
    "date_to": "2024-12-31"
  },
  "project_path": "/home/user/myproject"
}
```

**Filter operators:**

| Operator   | Example                       | Description                 |
| ---------- | ----------------------------- | --------------------------- |
| `=`        | `"category": "automation"`    | Exact match                 |
| `!=`       | `"category": "!test"`         | Not equal                   |
| `>`        | `"version": ">1.0.0"`         | Greater than (semver-aware) |
| `>=`       | `"version": ">=1.0.0"`        | Greater or equal            |
| `<`        | `"version": "<2.0.0"`         | Less than                   |
| `<=`       | `"version": "<=2.0.0"`        | Less or equal               |
| `in`       | `"category": ["auto", "web"]` | In list                     |
| `contains` | `"tags": "beta"`              | Contains value              |

**Date format:** ISO 8601 (`YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS`)

---

### 7. Fuzzy Matching

Handle typos and approximate matches:

```json
{
  "item_type": "tool",
  "query": "scaper",
  "fuzzy": {
    "enabled": true,
    "max_distance": 1
  },
  "project_path": "/home/user/myproject"
}
```

**Distance values:**

| Distance | Matches                  | Example                                |
| -------- | ------------------------ | -------------------------------------- |
| `0`      | Exact match only         | `scraper` → `scraper`                  |
| `1`      | One character difference | `scaper` → `scraper`, `http` → `https` |
| `2`      | Two characters           | `scapr` → `scraper`                    |

**Algorithm:** Levenshtein distance on individual words

---

### 8. Proximity Search

Find words within a window:

```json
{
  "item_type": "knowledge",
  "query": "lead generation",
  "proximity": {
    "enabled": true,
    "max_distance": 5
  },
  "project_path": "/home/user/myproject"
}
```

| Distance | Matches Example                            | Result           |
| -------- | ------------------------------------------ | ---------------- |
| 0        | `"Lead generation is important"`           | ✅ Adjacent      |
| 2        | `"Lead and customer generation"`           | ✅ 2 words apart |
| 5        | `"Lead, sales, and, customer, generation"` | ✅ 5 words apart |
| 10       | `"Lead is important. Also generation..."`  | ❌ Too far       |

---

## Sorting

Control result ordering:

```json
{
  "item_type": "tool",
  "query": "scrape",
  "sort_by": "score", // "score", "date", "name"
  "project_path": "/home/user/myproject"
}
```

| Sort Mode | Description          | Field Used                   |
| --------- | -------------------- | ---------------------------- |
| `score`   | Relevance (default)  | Calculated relevance score   |
| `date`    | Creation/update time | `updated_at` or `created_at` |
| `name`    | Alphabetical         | `name` or `id`               |

**Sort priority (tie-breaking):**

1. Primary sort field
2. Source priority: `project` → `user` → `registry`
3. Item ID (alphabetical)

---

## Pagination

Control result window:

```json
{
  "item_type": "tool",
  "query": "scrape",
  "limit": 10,
  "offset": 0,
  "project_path": "/home/user/myproject"
}
```

| Parameter | Default | Description                        |
| --------- | ------- | ---------------------------------- |
| `limit`   | 10      | Maximum results to return          |
| `offset`  | 0       | Starting position (for pagination) |

**Example:**

```json
{
  "query": "scrape",
  "limit": 10,
  "offset": 0 // Results 1-10
}
```

```json
{
  "query": "scrape",
  "limit": 10,
  "offset": 10 // Results 11-20
}
```

---

## Search Algorithm

### Keyword Matching

The search tool uses **BM25-inspired keyword matching**:

1. **Tokenization** - Split query and documents into words
2. **Query parsing** - Parse boolean operators, wildcards, phrases
3. **Field matching** - Match against title, description, content
4. **Field weighting** - Title matches score higher than content
5. **Relevance scoring** - Combine field scores with weights
6. **Ranking** - Sort by relevance score

### Field Weights (Default)

| Field         | Weight | Rationale                                |
| ------------- | ------ | ---------------------------------------- |
| `title`       | 3.0    | Most important - exact title matches     |
| `name`        | 3.0    | Tool/directive names are key identifiers |
| `description` | 2.0    | Summaries are highly relevant            |
| `category`    | 1.5    | Category helps narrow results            |
| `content`     | 1.0    | Full content is least specific           |

### Scoring Formula

```
score = (title_matches × 3.0) +
        (name_matches × 3.0) +
        (description_matches × 2.0) +
        (category_matches × 1.5) +
        (content_matches × 1.0)

normalized_score = score / max_possible_score
```

---

## Common Patterns

### Pattern 1: Discover All Tools

```json
{
  "item_type": "tool",
  "query": "",
  "limit": 100,
  "project_path": "/home/user/myproject"
}
```

### Pattern 2: Search by Category

```json
{
  "item_type": "tool",
  "query": "",
  "filters": {
    "category": "automation"
  },
  "project_path": "/home/user/myproject"
}
```

### Pattern 3: Find HTTP Scraping Tools

```json
{
  "item_type": "tool",
  "query": "scrap* AND (http OR api)",
  "filters": {
    "category": "automation"
  },
  "project_path": "/home/user/myproject"
}
```

### Pattern 4: Find Recent Knowledge

```json
{
  "item_type": "knowledge",
  "query": "api patterns",
  "filters": {
    "date_from": "2024-01-01"
  },
  "sort_by": "date",
  "project_path": "/home/user/myproject"
}
```

### Pattern 5: Search by Title Only

```json
{
  "item_type": "tool",
  "fields": {
    "title": "scraper"
  },
  "project_path": "/home/user/myproject"
}
```

### Pattern 6: Exclude Deprecated Items

```json
{
  "item_type": "tool",
  "query": "scrape",
  "filters": {
    "tags": "!deprecated"
  },
  "project_path": "/home/user/myproject"
}
```

### Pattern 7: Fuzzy Search for Typos

```json
{
  "item_type": "tool",
  "query": "scaper",
  "fuzzy": {
    "enabled": true,
    "max_distance": 1
  },
  "project_path": "/home/user/myproject"
}
```

### Pattern 8: Find Directives by Description

```json
{
  "item_type": "directive",
  "fields": {
    "description": "bootstrap project"
  },
  "project_path": "/home/user/myproject"
}
```

---

## Source Locations

| Source    | Path                               | Description            |
| --------- | ---------------------------------- | ---------------------- |
| `project` | `{project_path}/.ai/`              | Project-local items    |
| `user`    | `~/.ai/` (or `USER_SPACE` env var) | User-global items      |
| `system`  | `{install_location}/.ai/`          | Pre-packaged RYE tools |

**Search Order:**

| Source    | Search Locations                   | Priority                  |
| --------- | ---------------------------------- | ------------------------- |
| `project` | `{project_path}/.ai/`              | N/A                       |
| `user`    | `~/.ai/` (or `USER_SPACE` env var) | N/A                       |
| `all`     | All above (project + user)         | Results from both, merged |
| `system`  | `{install_location}/.ai/`          | N/A (search system tools) |

---

## Handler Dispatch

Search dispatches to the appropriate handler based on `item_type`:

```
mcp__rye__search(item_type="directive", query="...", project_path="...")
    │
    └─→ DirectiveHandler.search(query, source, limit, filters)
        │
        └─→ Keyword search (BM25-inspired)
            │
            ├─→ Query parsing (boolean, wildcards, phrases)
            ├─→ Field matching
            ├─→ Fuzzy matching (if enabled)
            ├─→ Proximity matching (if enabled)
            └─→ Returns ranked results
```

## Performance

### Keyword Search Performance

| Dataset Size | Search Time | Memory   |
| ------------ | ----------- | -------- |
| 100 items    | ~5ms        | Minimal  |
| 1,000 items  | ~10ms       | Low      |
| 10,000 items | ~50ms       | Moderate |

**Characteristics:**

- ✅ Fast for small to medium datasets
- ✅ No external dependencies
- ✅ Works offline
- ✅ Predictable performance
- ✅ Field indexing for fast lookups

**Optimizations:**

- Field indexing with in-memory caches
- Token caching for repeated queries
- Lazy loading of full content

---

## Limitations

### By Design

1. **Keyword-only** - No semantic understanding
   - Misses synonyms (e.g., "car" won't match "automobile")
   - Misses related concepts (e.g., "API" won't match "REST endpoint")
   - Exact word matching only

2. **No ranking intelligence** - Simple field weighting
   - Doesn't learn from user behavior
   - No personalization
   - Fixed scoring formula

3. **Fuzzy search limited** - Small character distance
   - Max distance of 2 to avoid noise
   - No stemming by default (opt-in only)
   - No lemmatization

---

## Error Responses

### Invalid Query Syntax

```json
{
  "error": "Invalid query syntax",
  "details": "Unmatched opening parenthesis at position 5",
  "position": 5
}
```

### Invalid Filter Value

```json
{
  "error": "Invalid filter value",
  "field": "version",
  "value": "latest",
  "expected": "Semantic version format (e.g., 1.0.0)"
}
```

### Unsupported Filter Operator

```json
{
  "error": "Unsupported filter operator",
  "field": "category",
  "operator": "contains",
  "supported": ["=", "!=", "in", "contains"]
}
```

---

## Related Documentation

- **MCP Server:** `[[rye/mcp-server]]` - MCP server architecture
- **MCP Tools:**
  - `[[rye/mcp-tools/execute]]` - Execute items
  - `[[rye/mcp-tools/load]]` - Load item content
  - `[[rye/mcp-tools/sign]]` - Sign items
  - `[[rye/mcp-tools/help]]` - Get usage guidance
- **MCP Server:** `[[rye/mcp-server]]` - MCP server architecture
- **Load Tool:** `[[rye/mcp-tools/load]]` - Load item content
- **Execute Tool:** `[[rye/mcp-tools/execute]]` - Execute items
