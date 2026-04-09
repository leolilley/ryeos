"""Primary action descriptions — single source of truth.

Used by rye-mcp/server.py (MCP transport layer) and
rye/rye/.ai/tools/rye/primary/ (in-thread primary actions).
"""

# ---------------------------------------------------------------------------
# Shared field descriptions (used across multiple tools)
# ---------------------------------------------------------------------------

PROJECT_PATH_DESC = "Absolute path to the project root containing .ai/."

ITEM_ID_DESC = (
    "<description>"
    "Slash-separated path without file extension. "
    "Resolved project → user → system. If unsure of the ID, use fetch in query mode."
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
    "If ambiguous between tool and directive, use a canonical ref (tool:id or directive:id). "
    "Executing a directive returns your_directions and a body — follow them. "
    "Executing a tool runs it. Knowledge is not executable — use rye fetch."
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
    "<rule>When the user says 'rye execute [name]', call execute DIRECTLY with the given name as item_id. "
    "Do NOT call fetch first — go straight to execute. "
    "Only call fetch first if the user does NOT provide a name and you need to discover it.</rule>"
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
    '<example>Spawn directive as thread: item_id="my/workflow", parameters={"target": "value"}, thread="fork"</example>'
    '<example>Spawn async thread: item_id="my/workflow", parameters={"target": "value"}, thread="fork", async=true</example>'
    '<example>Execute remotely: item_id="directive:my/workflow", parameters={"target": "staging"}, target="remote"</example>'
    "</examples>"
)

EXECUTE_PARAMETERS_DESC = (
    "<description>"
    "Parameters passed to the item. For directives, these are input values. "
    "For tools, these are tool-specific parameters."
    "</description>"
    "<rules>"
    "<rule>When the user provides extra words after the name, those ARE parameter values — do NOT ask for clarification, pass them as parameters.</rule>"
    "<rule>If unsure which parameter key they map to, call fetch on the item first to see its input schema.</rule>"
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
    "Execution mode for directives."
    "</description>"
    "<rules>"
    '<rule>"inline" (default) — returns your_directions for the calling agent to follow directly</rule>'
    '<rule>"fork" — spawn as a managed thread with its own LLM loop</rule>'
    "</rules>"
)

EXECUTE_TARGET_DESC = (
    "<description>"
    "Where to execute: locally or on a remote server."
    "</description>"
    "<rules>"
    '<rule>"local" (default) — execute in the current environment</rule>'
    '<rule>"remote" — execute on ryeos-node server. Configure via .ai/config/remotes/remotes.yaml: '
    'add a "default" entry under remotes: with url and key_env (env var name holding the API key). '
    'Use "remote:name" to target a specific remote.</rule>'
    "</rules>"
)

EXECUTE_ASYNC_DESC = (
    "<description>"
    "Return immediately with thread_id (fire-and-forget). "
    "false (default) blocks until complete. Applies to fork and remote modes."
    "</description>"
)

EXECUTE_RESUME_THREAD_ID_DESC = (
    "<description>"
    "Thread ID to resume. Routes to the original callee with resume params. "
    "Directives resume via transcript reconstruction, graphs via checkpoint reload."
    "</description>"
)

# ---------------------------------------------------------------------------
# fetch
# ---------------------------------------------------------------------------

FETCH_TOOL_DESC = (
    "<description>"
    "Resolve a name to items. Two modes: give an item_id to get content, "
    "or give a query+scope to discover matches. "
    "item_id is a slash-separated path without extension, "
    "resolved project → user → system unless source restricts it. "
    "Accepts canonical refs (tool:id, directive:id, knowledge:id) for explicit type scoping."
    "</description>"
    "<rules>"
    "<rule>Re-sign after copying or editing any item.</rule>"
    "<rule>Use this to inspect an item's input schema before calling execute.</rule>"
    '<rule>System space includes built-in knowledge entries covering how Rye works — '
    'use query mode with scope="knowledge" when you need to understand a concept, '
    "resolve ambiguity, or figure out how something is configured.</rule>"
    "<rule>The knowledge base is your reference manual. Search it before asking the user.</rule>"
    "</rules>"
)

FETCH_SCOPE_DESC = (
    "<description>"
    "Item type and optional namespace filter. Query mode only."
    "</description>"
    "<examples>"
    '<example>Shorthand: "directive", "tool", "knowledge", "tool.rye.core.*"</example>'
    '<example>Capability format: "rye.fetch.directive.*", "rye.fetch.tool.rye.core.*"</example>'
    "</examples>"
    "<rules>"
    "<rule>Namespace dots map to path separators; trailing .* matches all items under that prefix.</rule>"
    "</rules>"
)

FETCH_QUERY_DESC = (
    "<description>"
    "Keyword search query. Triggers query mode. Supports AND, OR, NOT, quoted phrases, and * wildcards."
    "</description>"
    "<rules>"
    '<rule>Use "*" to list all items in a scope.</rule>'
    "</rules>"
)

FETCH_SOURCE_DESC = (
    '<description>Restrict where to resolve from. ID mode: "project", "user", "system", or "registry". '
    'Query mode: "project", "user", "system", "local" (all local spaces), '
    '"registry" (published items only), or "all" (local + registry, default).</description>'
)

FETCH_DESTINATION_DESC = (
    "<description>"
    'Copy the item to this space after resolving: "project" or "user". '
    "ID mode only. Use to customize system items."
    "</description>"
    "<rules>"
    "<rule>Re-sign after copying or editing.</rule>"
    "</rules>"
)

FETCH_LIMIT_DESC = "Maximum number of results to return. Query mode only."

# ---------------------------------------------------------------------------
# sign
# ---------------------------------------------------------------------------

SIGN_TOOL_DESC = (
    "<description>"
    "Validate structure and write an Ed25519 signature to a Rye item file. "
    "Run after any edit or copy."
    "</description>"
    "<rules>"
    "<rule>item_id accepts canonical refs (tool:id, directive:id, knowledge:id, config:id).</rule>"
    "<rule>item_id supports glob patterns for batch signing, including with canonical refs.</rule>"
    "<rule>System space items are immutable — copy to project or user first.</rule>"
    "</rules>"
    "<examples>"
    '<example>"tool:my-project/workflows/deploy" — sign a single tool</example>'
    '<example>"directive:*" — batch sign all directives</example>'
    '<example>"tool:rye/core/*" — batch sign all tools under a prefix</example>'
    "</examples>"
)

SIGN_ITEM_ID_DESC = (
    "<description>"
    "Item path or glob pattern. Supports * and ? wildcards. "
    "Accepts canonical refs (tool:id, directive:id, knowledge:id, config:id)."
    "</description>"
    "<examples>"
    '<example>Single: "tool:my-project/workflows/deploy"</example>'
    '<example>Batch: "directive:my-project/workflows/*" or "tool:*" (all items of that kind)</example>'
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
