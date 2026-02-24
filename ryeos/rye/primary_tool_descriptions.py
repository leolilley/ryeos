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
    "<description>"
    "Slash-separated path without file extension. "
    "Resolved project → user → system. If unsure of the ID, call search first."
    "</description>"
    "<examples>"
    '<example>"init" → .ai/directives/init.md</example>'
    '<example>"rye/core/create_directive" → .ai/directives/rye/core/create_directive.md</example>'
    '<example>"rye/bash/bash" → .ai/tools/rye/bash/bash.py</example>'
    "</examples>"
)

# ---------------------------------------------------------------------------
# execute
# ---------------------------------------------------------------------------

EXECUTE_TOOL_DESC = (
    "<description>"
    "Run a Rye item. item_id is a slash-separated path without extension. "
    "Resolved project → user → system. If you don't know the ID, call search first. "
    "Executing a directive returns parsed steps with an instructions field — follow them. "
    "Set thread=true to spawn a managed thread instead. "
    "Executing a tool runs it. Executing knowledge returns context."
    "</description>"
    "<rules>"
    "<rule>ALWAYS call this tool when the user says 'rye execute' — NEVER ask for clarification.</rule>"
    "<rule>Pass any extra words the user provides as parameters.</rule>"
    "<rule>The tool rejects unknown parameter keys and returns the list of valid ones — let it self-correct.</rule>"
    "</rules>"
    "<examples>"
    '<example>User says "rye execute init project" → call with item_id="init", parameters={"project_type": "project"}</example>'
    '<example>User says "rye execute init" → call with item_id="init", parameters={}</example>'
    "</examples>"
)

EXECUTE_PARAMETERS_DESC = (
    "<description>"
    "Parameters passed to the item. For directives, these are input values. "
    "For tools, these are tool-specific parameters."
    "</description>"
    "<rules>"
    "<rule>When the user provides extra words after the directive name, those ARE parameter values — do NOT ask for clarification, pass them as parameters.</rule>"
    "<rule>If unsure which parameter key they map to, call load on the item first to see its input schema.</rule>"
    "<rule>Unknown keys are rejected with the list of valid inputs — safe to guess and let the tool correct you.</rule>"
    "</rules>"
    "<examples>"
    '<example>{"name": "my_tool"}</example>'
    '<example>{"space": "project"}</example>'
    "</examples>"
)

EXECUTE_DRY_RUN_DESC = (
    "<description>"
    "Validate without executing. Directives: parse and check inputs. "
    "Tools: build and validate the executor chain."
    "</description>"
)

# ---------------------------------------------------------------------------
# search
# ---------------------------------------------------------------------------

SEARCH_TOOL_DESC = (
    "<description>"
    "Discover item IDs before calling execute or load. Searches directives, tools, "
    "or knowledge across project/user/system spaces. Returns matching IDs you can "
    "pass to other tools."
    "</description>"
    "<rules>"
    "<rule>Use scope to set the item type — shorthand or capability format. Dots in the namespace become path separators.</rule>"
    '<rule>System space includes built-in knowledge entries covering how Rye works — search with scope="knowledge" when you need to understand a concept, resolve ambiguity, or figure out how something is configured.</rule>'
    "<rule>The knowledge base is your reference manual. Search it before asking the user.</rule>"
    "</rules>"
    "<examples>"
    '<example>scope="directive", query="init"</example>'
    '<example>scope="tool.rye.core.*", query="*"</example>'
    '<example>scope="knowledge", query="spaces" — find knowledge about how spaces work</example>'
    "</examples>"
)

SEARCH_SCOPE_DESC = (
    "<description>"
    "Item type and optional namespace filter."
    "</description>"
    "<examples>"
    '<example>Shorthand: "directive", "tool", "knowledge", "tool.rye.core.*"</example>'
    '<example>Capability format: "rye.search.directive.*", "rye.search.tool.rye.core.*"</example>'
    "</examples>"
    "<rules>"
    "<rule>Namespace dots map to path separators; trailing .* matches all items under that prefix.</rule>"
    "</rules>"
)

SEARCH_QUERY_DESC = (
    "<description>"
    "Keyword search query. Supports AND, OR, NOT, quoted phrases, and * wildcards."
    "</description>"
    "<rules>"
    '<rule>Use "*" to list all items in a scope.</rule>'
    "</rules>"
)

SEARCH_SPACE_DESC = (
    '<description>Which spaces to search: "project", "user", "system", or "all" (default).</description>'
)

SEARCH_LIMIT_DESC = "Maximum number of results to return."

# ---------------------------------------------------------------------------
# load
# ---------------------------------------------------------------------------

LOAD_TOOL_DESC = (
    "<description>"
    "Read raw content and metadata of a Rye item for inspection. Also copies items "
    "between spaces — set destination to copy a system item into project or user space "
    "for customization. item_id is a slash-separated path without extension, "
    "resolved project → user → system unless source restricts it."
    "</description>"
    "<rules>"
    "<rule>Re-sign after copying or editing any item.</rule>"
    "<rule>Use this to inspect an item's input schema before calling execute.</rule>"
    "</rules>"
)

LOAD_SOURCE_DESC = (
    "<description>"
    'Restrict where to load from: "project", "user", or "system". '
    "If omitted, resolves project → user → system (first match wins)."
    "</description>"
)

LOAD_DESTINATION_DESC = (
    "<description>"
    'Copy the item to this space after loading: "project" or "user". '
    "Use to customize system items."
    "</description>"
    "<rules>"
    "<rule>Re-sign after copying or editing.</rule>"
    "</rules>"
)

# ---------------------------------------------------------------------------
# sign
# ---------------------------------------------------------------------------

SIGN_TOOL_DESC = (
    "<description>"
    "Validate structure and write an Ed25519 signature to a Rye item file. "
    "Run after any edit or copy."
    "</description>"
    "<rules>"
    "<rule>item_id supports glob patterns for batch signing.</rule>"
    "<rule>System space items are immutable — copy to project or user first.</rule>"
    "</rules>"
    "<examples>"
    '<example>"my-project/workflows/deploy" — sign a single item</example>'
    '<example>"my-project/workflows/*" — batch sign all items under a prefix</example>'
    '<example>"*" — sign all items of that type</example>'
    "</examples>"
)

SIGN_ITEM_ID_DESC = (
    "<description>"
    "Item path or glob pattern. Supports * and ? wildcards."
    "</description>"
    "<examples>"
    '<example>Single: "my-project/workflows/deploy"</example>'
    '<example>Batch: "my-project/workflows/*" or "*" (all items of that type)</example>'
    "</examples>"
)

SIGN_SOURCE_DESC = (
    "<description>"
    'Where the item lives: "project" (default) or "user".'
    "</description>"
    "<rules>"
    "<rule>System items cannot be signed — copy them first.</rule>"
    "</rules>"
)
