"""Primary tool descriptions — single source of truth.

Used by rye-mcp/server.py (MCP transport layer) and
rye/rye/.ai/tools/rye/primary/ (in-thread primary tools).
"""

# ---------------------------------------------------------------------------
# Shared field descriptions (used across multiple tools)
# ---------------------------------------------------------------------------

ITEM_TYPE_DESC = 'What to operate on: "directive", "tool", or "knowledge".'

PROJECT_PATH_DESC = "Absolute path to the project root containing .ai/."

ITEM_ID_DESC = (
    "Slash-separated path without file extension. "
    'Examples: "rye/core/init" → .ai/directives/rye/core/init.md, '
    '"rye/bash/bash" → .ai/tools/rye/bash/bash.py. '
    "If unsure of the ID, use search to discover it first."
)

# ---------------------------------------------------------------------------
# execute
# ---------------------------------------------------------------------------

EXECUTE_TOOL_DESC = (
    "Run a Rye item. item_id is a slash-separated path without extension "
    '(e.g. "rye/core/init" resolves to .ai/directives/rye/core/init.md). '
    "Resolved project → user → system. If you don't know the ID, call search first. "
    "Executing a directive returns parsed steps with an instructions field — follow them. "
    "Executing a tool runs it. Executing knowledge returns context."
)

EXECUTE_PARAMETERS_DESC = (
    "Parameters passed to the item. For directives, these are "
    'input values (e.g. {"name": "my_tool"}). For tools, '
    "these are tool-specific parameters."
)

EXECUTE_DRY_RUN_DESC = (
    "Validate without executing. Directives: parse and check inputs. "
    "Tools: build and validate the executor chain."
)

# ---------------------------------------------------------------------------
# search
# ---------------------------------------------------------------------------

SEARCH_TOOL_DESC = (
    "Discover item IDs before calling execute or load. Searches directives, tools, "
    "or knowledge across project/user/system spaces. Returns matching IDs you can "
    "pass to other tools. Use scope to set the item type — shorthand: "
    '"directive", "tool", "knowledge", "tool.rye.core.*" — or capability format: '
    '"rye.search.directive.*". Dots in the namespace become path separators.'
)

SEARCH_SCOPE_DESC = (
    "Item type and optional namespace filter. "
    'Shorthand: "directive", "tool", "knowledge", "tool.rye.core.*". '
    'Capability format: "rye.search.directive.*", "rye.search.tool.rye.core.*". '
    "Namespace dots map to path separators; trailing .* matches all items under that prefix."
)

SEARCH_QUERY_DESC = (
    "Keyword search query. Supports AND, OR, NOT, quoted phrases, and * wildcards. "
    'Use "*" to list all items in a scope.'
)

SEARCH_SPACE_DESC = (
    'Which spaces to search: "project", "user", "system", or "all" (default).'
)

SEARCH_LIMIT_DESC = "Maximum number of results to return."

# ---------------------------------------------------------------------------
# load
# ---------------------------------------------------------------------------

LOAD_TOOL_DESC = (
    "Read raw content and metadata of a Rye item for inspection. Also copies items "
    "between spaces — set destination to copy a system item into project or user space "
    "for customization (re-sign after). item_id is a slash-separated path without extension, "
    "resolved project → user → system unless source restricts it."
)

LOAD_SOURCE_DESC = (
    'Restrict where to load from: "project", "user", or "system". '
    "If omitted, resolves project → user → system (first match wins)."
)

LOAD_DESTINATION_DESC = (
    'Copy the item to this space after loading: "project" or "user". '
    "Use to customize system items. Re-sign after copying or editing."
)

# ---------------------------------------------------------------------------
# sign
# ---------------------------------------------------------------------------

SIGN_TOOL_DESC = (
    "Validate structure and write an Ed25519 signature to a Rye item file. "
    "Run after any edit or copy. item_id supports glob patterns for batch signing "
    '(e.g. "my-project/workflows/*" or "*" for all). '
    "System space items are immutable — copy to project or user first."
)

SIGN_ITEM_ID_DESC = (
    "Item path or glob pattern. "
    'Single: "my-project/workflows/deploy". '
    'Batch: "my-project/workflows/*" or "*" (all items of that type). '
    "Supports * and ? wildcards."
)

SIGN_SOURCE_DESC = (
    'Where the item lives: "project" (default) or "user". '
    "System items cannot be signed — copy them first."
)
