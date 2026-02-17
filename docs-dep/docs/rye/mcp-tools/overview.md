# RYE MCP Tools Overview

**Source:** RYE's 4 MCP tools that wrap executor

---

## Overview

RYE exposes exactly **4 MCP tools** that work with **3 item types**. This is the unified interface that LLMs interact with.

The key insight: LLMs see only 4 tools, not dozens of individual operations. This simplifies the interface while maintaining full capability through the item type system.

---

## The 4 MCP Tools

| Tool | Purpose | Primary Use |
|------|---------|-------------|
| `mcp__rye__search` | Search for items by query | Find directives, tools, or knowledge by keywords |
| `mcp__rye__load` | Load item content | Read-only inspection or copy between locations |
| `mcp__rye__execute` | Execute an item | Run tools, parse directives, load knowledge |
| `mcp__rye__sign` | Validate and sign item files | Add integrity hashes |

---

## The 3 Item Types

| Item Type | Location | Format | Description |
|-----------|----------|--------|-------------|
| `directive` | `.ai/directives/` | XML in Markdown | Workflow definitions (HOW to accomplish tasks) |
| `tool` | `.ai/tools/` | Python, YAML, etc. | Executable scripts (DO the actual work) |
| `knowledge` | `.ai/knowledge/` | Markdown + frontmatter | Domain information, patterns, learnings |

---

## Key Architecture Points

1. **LLM only sees 4 tools** — Not dozens of individual tools
2. **On-demand loading** — Items are loaded when requested, not at startup
3. **Unified interface** — Same 5 tools work with all 3 item types
4. **No auto-discovery to MCP** — Tools in `.ai/tools/` are NOT exposed directly to MCP
5. **RYE adds intelligence** — Content understanding on top of Lilux primitives

---

## Tool Details

### Search

Search for items with keyword matching, boolean operators, wildcards, and fuzzy matching. Supports filtering by metadata (category, version, tags, date ranges), field-specific search, proximity search, and pagination.

```python
mcp__rye__search(
    item_type="tool",           # "directive", "tool", "knowledge", or "all"
    query="git operations",     # Natural language or keyword query
    project_path="/path/to/project",
    source="project",           # "project", "user", or "all" (default: "project")
    limit=10,                   # Maximum results
    sort_by="score",            # "score", "date", or "name"
    category="git",             # Filter by category (optional)
    tags=["vcs"],               # Filter by tags (optional)
    date_from="2024-01-01",     # Filter by date range (optional)
    date_to="2024-12-31"
)
```

**Returns:** List of matching items with scores, titles, and metadata.

**RYE Intelligence:**
- Detects content type from file path
- Extracts titles from XML/frontmatter
- Computes relevance scores using BM25-inspired keyword matching
- Supports boolean operators (AND, OR, NOT)
- Supports wildcards and phrase search
- Supports field-specific search (title, description, content)
- Supports fuzzy matching for typos
- Supports proximity search for word windows
- Filters by metadata (category, version, tags, date ranges)
- Sorts by score, date, or name
- Supports pagination with limit/offset

---

### Load

Load item content for inspection or copy between locations.

```python
mcp__rye__load(
    item_type="tool",
    item_id="git",
    project_path="/path/to/project",
    source="project",           # "project" or "user"
    destination="user"          # Where to copy (optional, omit for read-only)
)
```

**Behavior:**
- `destination` omitted or equals `source`: **Read-only mode** — returns content without copying
- `destination` differs from `source`: **Copy mode** — copies item to destination location

**RYE Intelligence:**
- Validates content after loading
- Resolves tool chains with space precedence (project → user → system)
- Parses XML/frontmatter/metadata for each item type

---

### Execute

Execute items with orchestration and telemetry.

```python
mcp__rye__execute(
    item_type="tool",
    item_id="git",
    project_path="/path/to/project",
    parameters={"command": "status"},
    dry_run=False               # True = validate only
)
```

**Behavior by item type:**

| Item Type | Execute Behavior |
|-----------|-----------------|
| `directive` | Parses XML, returns process steps for agent to follow |
| `tool` | Executes via Lilux PrimitiveExecutor (subprocess, HTTP, chains) |
| `knowledge` | Loads content for agent reference |

**RYE Intelligence:**
- Validates required inputs for directives
- Resolves tool chains before execution
- Tracks execution telemetry (duration, success/failure)
- Manages concurrency locks

---

### Sign

Validate and sign item files with integrity hashes.

```python
mcp__rye__sign(
    item_type="directive",
    item_id="bootstrap",
    project_path="/path/to/project",
    location="project",         # "project" or "user"
    category="core"            # Category folder (optional)
)
```

**What signing does:**
1. Validates item structure (required fields, format)
2. Computes SHA256 integrity hash (pure Python `hashlib`)
3. Adds signature comment to file

**Signature format:**
```
<!-- kiwi-mcp:valid:{hash}:{signature}:{item_id} -->
```

**RYE Intelligence:**
- Validates directive XML structure
- Validates tool metadata headers
- Validates knowledge frontmatter
- Supports re-signing (removes old signature before computing new hash)

---

## Implementation Location

The 4 MCP tools are implemented in the `rye/` package (Python code), NOT in `.ai/tools/`.

```
rye/rye/
├── server.py           # MCP server entry point, tool registration
├── tools/
│   ├── search.py       # SearchTool class
│   ├── load.py         # LoadTool class
│   ├── execute.py      # ExecuteTool class
│   └── sign.py         # SignTool class
└── handlers/           # Type-specific handlers (if needed)
```

---

## Distinction from Data-Driven Tools

| Aspect | 4 MCP Tools | Data-Driven Tools |
|--------|-------------|-------------------|
| **Location** | `rye/rye/tools/` (Python code) | `.ai/tools/` (data files) |
| **Exposure** | Directly exposed to MCP | Accessed via `mcp__rye__execute` |
| **Discovery** | Hardcoded in server.py | Loaded on demand by search/load |
| **Purpose** | Universal interface | User-defined operations |
| **Who creates** | RYE developers | Users/agents |

---

## Source/Destination Resolution

RYE tools support two locations:

| Source/Destination | Path | Description |
|--------------------|------|-------------|
| `project` | `./.ai/` | Project-local items |
| `user` | `~/.ai/` | User-space items (shared across projects) |

**Resolution order for search:**
1. Project space (`.ai/`)
2. User space (`~/.ai/`)

## Related Tools

For registry operations (searching, downloading, publishing items), see the `.ai/tools/rye/registry` tool, accessed via `execute`.

---

## Tool Spaces and Mutability

RYE uses a three-space model for tool resolution:

| Space    | Location                              | Mutability | Precedence |
| --------- | ------------------------------------- | ------------ | ----------- |
| **Project** | `{project}/.ai/`                   | Mutable     | 1 (highest)  |
| **User**    | `~/.ai/`                            | Mutable     | 2 (medium)   |
| **System**  | `site-packages/rye/.ai/`          | Immutable   | 3 (lowest)   |

**Shadowing Behavior:** Users can shadow system tools by creating同名工具 in project or user space. This is intentional - allows customization and experimentation. The `load` tool can copy system tools to project/user space for modification.

**See Also:** [[../executor/tool-resolution-and-validation.md]] for complete details on tool spaces, precedence, chain validation, and shadowing behavior.

---

## Related Documentation

- [[../executor/overview]] — How tools are executed via Lilux primitives
- [[../mcp-server]] — MCP server configuration and startup
- [[../package/structure]] — Package organization
- [[../../lilux/primitives/overview]] — Lilux execution primitives
