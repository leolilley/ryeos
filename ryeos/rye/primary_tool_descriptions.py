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
    "Resolved project → user → system. "
    "Executing a directive returns your_directions and a body — follow them. "
    "Executing a tool runs it. Executing knowledge returns context."
    "</description>"
    "<threading>"
    "To spawn a directive as a managed thread (with its own LLM loop), pass thread=\"fork\" as a top-level parameter. "
    "Threading parameters (all in the parameters object):"
    "<param>model: string — override the LLM model for the thread</param>"
    "<param>limit_overrides: object — override limits e.g. {\"turns\": 10, \"spend\": 0.10}</param>"
    "Directive inputs go directly in parameters alongside model/limit_overrides. "
    "Example: parameters={\"niche\": \"plumbers\", \"city\": \"Dunedin\"} "
    "The framework auto-injects parent_thread_id, parent_depth, and parent_capabilities — never pass these manually."
    "</threading>"
    "<rules>"
    "<rule>When the user says 'rye execute [type] [name]', call execute DIRECTLY with the given name as item_id. "
    "Do NOT call search or load first — go straight to execute. "
    "Only call search first if the user does NOT provide a name and you need to discover it.</rule>"
    "<rule>ALWAYS call this tool when the user says 'rye execute' — NEVER ask for clarification.</rule>"
    "<rule>Pass any extra words the user provides as parameters.</rule>"
    "<rule>The tool rejects unknown parameter keys and returns the list of valid ones — let it self-correct.</rule>"
    "<rule>When executing a directive, the response contains a your_directions field and a body field. "
    "Read your_directions, then follow the body immediately. Do NOT summarize, describe, or re-call execute. "
    "Output render blocks verbatim. Follow instruction blocks silently.</rule>"
    "</rules>"
    "<examples>"
    '<example>User says "rye execute init project" → call with item_id="init", parameters={"project_type": "project"}</example>'
    '<example>User says "rye execute init" → call with item_id="init", parameters={}</example>'
    '<example>Spawn directive as thread: item_type="directive", item_id="my/workflow", parameters={"target": "value"}, thread="fork"</example>'
    '<example>Spawn async thread: item_type="directive", item_id="my/workflow", parameters={"target": "value"}, thread="fork", async=true</example>'
    '<example>Execute remotely: item_type="directive", item_id="my/workflow", parameters={"target": "staging"}, thread="remote"</example>'
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
    '<example>{"model": "sonnet"}</example>'
    "</examples>"
)

EXECUTE_DRY_RUN_DESC = (
    "<description>"
    "Validate without executing. Directives: parse and check inputs. "
    "Tools: build and validate the executor chain."
    "</description>"
)

EXECUTE_THREAD_DESC = (
    "<description>"
    "Execution mode for directives and tools (ignored for knowledge)."
    "</description>"
    "<rules>"
    '<rule>"inline" (default) — returns your_directions for the calling agent to follow directly</rule>'
    '<rule>"fork" — spawn as a managed thread with its own LLM loop</rule>'
    '<rule>"remote" — execute on ryeos-remote server. Configure via .ai/config/cas/remote.yaml: add a "default" entry under remotes: with url and key_env (env var name holding the API key). Use "remote:name" to target a specific remote.</rule>'
    "</rules>"
)

EXECUTE_ASYNC_DESC = (
    "<description>"
    "Return immediately with thread_id (fire-and-forget). "
    "false (default) blocks until complete. Applies to fork and remote modes."
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
    '<description>Which spaces to search: "project", "user", "system", "local" (all local spaces), '
    '"registry" (published items only), or "all" (local + registry, default).</description>'
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
    'Restrict where to load from: "project", "user", "system", or "registry". '
    "If omitted, resolves project → user → system (first match wins). "
    'Use "registry" to pull items from a remote registry by their full item_id '
    "(namespace/category/name format)."
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
