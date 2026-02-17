# RYE OS Directive Structure

**Date:** 2026-02-17
**Status:** Planned
**Scope:** All bundled directives for rye-os and rye-core packages

---

## Design Principles

1. **Every user-facing tool gets its own directive** — granular so any LLM pulls only the context it needs to execute one tool.
2. **Directives ARE the interface** — you use RYE by calling directives which instruct tool execution. The LLM is fed the directive and knows exactly how to call the tool.
3. **Category mirrors tool category** — directive at `rye/file-system/read.md` wraps tool at `rye/file-system/read.py`.
4. **rye-core has minimal directives** — only for the few core tools an LLM calls directly (system, telemetry, registry, bundler). rye-core is the execution engine; install it to use programmatically.
5. **Demos are NOT bundled** — they're full-depth showcase projects published to the registry as a separate bundle.

---

## Bundle Boundaries

### rye-core bundle (`pip install rye-core`)

Ships only `rye/core/*` items. Directives:

| Directive                         | Tool                                                 | Purpose                             |
| --------------------------------- | ---------------------------------------------------- | ----------------------------------- |
| `rye/core/system`                 | `rye/core/system/system.py`                          | Query paths, time, runtime info     |
| `rye/core/telemetry`              | `rye/core/telemetry/telemetry.py`                    | Read MCP server logs, stats, errors |
| `rye/core/bundler/create_bundle`  | `rye/core/bundler/bundler.py` (action: create)       | Create bundle manifest              |
| `rye/core/bundler/verify_bundle`  | `rye/core/bundler/bundler.py` (action: verify)       | Verify manifest signature + hashes  |
| `rye/core/bundler/inspect_bundle` | `rye/core/bundler/bundler.py` (action: inspect)      | Parse manifest without verification |
| `rye/core/bundler/list_bundles`   | `rye/core/bundler/bundler.py` (action: list)         | List all bundles under .ai/bundles/ |
| `rye/core/registry/login`         | `rye/core/registry/registry.py` (action: login)      | Start device auth flow              |
| `rye/core/registry/login_poll`    | `rye/core/registry/registry.py` (action: login_poll) | Poll for auth completion            |
| `rye/core/registry/logout`        | `rye/core/registry/registry.py` (action: logout)     | Clear local auth session            |
| `rye/core/registry/signup`        | `rye/core/registry/registry.py` (action: signup)     | Create account                      |
| `rye/core/registry/whoami`        | `rye/core/registry/registry.py` (action: whoami)     | Show authenticated user             |
| `rye/core/registry/search`        | `rye/core/registry/registry.py` (action: search)     | Search registry items               |
| `rye/core/registry/pull`          | `rye/core/registry/registry.py` (action: pull)       | Download item from registry         |
| `rye/core/registry/push`          | `rye/core/registry/registry.py` (action: push)       | Upload item to registry             |
| `rye/core/registry/delete`        | `rye/core/registry/registry.py` (action: delete)     | Remove item from registry           |
| `rye/core/registry/publish`       | `rye/core/registry/registry.py` (action: publish)    | Make item public                    |
| `rye/core/registry/unpublish`     | `rye/core/registry/registry.py` (action: unpublish)  | Make item private                   |

**17 directives** in rye-core.

Infrastructure tools that do NOT get directives (internal execution chain):

- `rye/core/runtimes/*` — python_script, python_function, bash, node, mcp_stdio, mcp_http
- `rye/core/primitives/*` — subprocess, http_client
- `rye/core/parsers/*` — markdown_xml, markdown_frontmatter, python_ast, yaml
- `rye/core/extractors/*` — directive/, tool/, knowledge/
- `rye/core/sinks/*` — file_sink, null_sink, websocket_sink

### rye-os bundle (`pip install rye-os`)

Ships all `rye/*` items. Includes everything in rye-core plus all categories below.

---

## Full Directive Map

### `rye/core/` — System introspection, bundler & registry

```
rye/core/
├── system.md                    # rye/core/system/system.py
│                                  items: paths, time, runtime, all
├── telemetry.md                 # rye/core/telemetry/telemetry.py
│                                  items: logs, stats, errors, all
│                                  env: RYE_LOG_LEVEL, USER_SPACE
├── bundler/
│   ├── create_bundle.md         # bundler.py action=create
│   ├── verify_bundle.md         # bundler.py action=verify
│   ├── inspect_bundle.md        # bundler.py action=inspect
│   └── list_bundles.md          # bundler.py action=list
└── registry/
    ├── login.md                 # registry.py action=login
    ├── login_poll.md            # registry.py action=login_poll
    ├── logout.md                # registry.py action=logout
    ├── signup.md                # registry.py action=signup
    ├── whoami.md                # registry.py action=whoami
    ├── search.md                # registry.py action=search
    ├── pull.md                  # registry.py action=pull
    ├── push.md                  # registry.py action=push
    ├── delete.md                # registry.py action=delete
    ├── publish.md               # registry.py action=publish
    └── unpublish.md             # registry.py action=unpublish
```

### `rye/primary/` — The 4 MCP tool wrappers

These are the most important directives. They teach the LLM how to call the four core MCP tools that everything else builds on.

```
rye/primary/
├── search.md                    # rye/primary/rye_search.py
│                                  scope, query, project_path, space, limit
├── load.md                      # rye/primary/rye_load.py
│                                  item_type, item_id, source, destination
├── execute.md                   # rye/primary/rye_execute.py
│                                  item_type, item_id, parameters, dry_run
└── sign.md                      # rye/primary/rye_sign.py
                                   item_type, item_id, source
```

### `rye/bash/` — Shell execution

```
rye/bash/
└── bash.md                      # rye/bash/bash.py
                                   command execution via subprocess
```

### `rye/file-system/` — File operations

```
rye/file-system/
├── read.md                      # rye/file-system/read.py
├── write.md                     # rye/file-system/write.py
├── edit_lines.md                # rye/file-system/edit_lines.py
├── ls.md                        # rye/file-system/ls.py
├── glob.md                      # rye/file-system/glob.py
└── grep.md                      # rye/file-system/grep.py
```

### `rye/web/` — Web tools

```
rye/web/
├── websearch.md                 # rye/web/websearch.py
└── webfetch.md                  # rye/web/webfetch.py
```

### `rye/lsp/` — Language server

```
rye/lsp/
└── lsp.md                       # rye/lsp/lsp.py
```

### `rye/mcp/` — External MCP integration

3 tools, but `manager.py` has 4 actions = 6 directives total.

```
rye/mcp/
├── connect.md                   # rye/mcp/connect.py
│                                  Execute tool call on MCP server
├── discover.md                  # rye/mcp/discover.py
│                                  Discover tools from MCP server
├── add_server.md                # rye/mcp/manager.py action=add
│                                  Register MCP server + auto-discover
├── list_servers.md              # rye/mcp/manager.py action=list
├── refresh_server.md            # rye/mcp/manager.py action=refresh
└── remove_server.md             # rye/mcp/manager.py action=remove
```

### `rye/agent/` — Threading & orchestration

```
rye/agent/
├── setup_provider.md            # Configure LLM provider (anthropic/openai YAML)
├── create_threaded_directive.md # Create directive with thread execution support
│                                  (moved from rye/core)
├── thread_directive.md          # rye/agent/threads/thread_directive.py
│                                  Execute a directive in a fresh thread
├── orchestrator.md              # rye/agent/threads/orchestrator.py
│                                  Multi-thread orchestration
└── threads/
    └── thread_summary.md        # Already exists
```

No directives for `rye/agent/permissions/` — internal to threading harness.
No directives for `rye/agent/providers/` — YAML configs, not directly called.

### `rye/authoring/` — Item creation workflows

These are higher-level directives that compose multiple primary tools (search, execute file-system/write, sign) to create items. Moved from `rye/core/`.

```
rye/authoring/
├── create_directive.md          # Create + validate + sign a directive
├── create_tool.md               # Create + validate + sign a tool
└── create_knowledge.md          # Create + validate + sign a knowledge entry
```

### `rye/onboarding/` — Init & guides

```
rye/onboarding/
├── init.md                      # System init: ~/.ai/ structure, provider config,
│                                  keys. Outputs command dispatch table for user
│                                  to paste into their AGENTS.md / CLAUDE.md / etc.
├── project_init.md              # Project init: .ai/ directory structure.
│                                  Outputs command dispatch table for the project.
│
└── guides/
    ├── basics/
    │   ├── searching_items.md       # How to search across spaces
    │   ├── loading_items.md         # How to load, inspect, copy items
    │   ├── executing_items.md       # How to execute directives, tools, knowledge
    │   └── signing_items.md         # How to sign and verify items
    ├── authoring/
    │   ├── writing_directives.md    # Directive structure, XML metadata, process steps
    │   ├── writing_tools.md         # Tool types, executor chain, CONFIG_SCHEMA
    │   └── writing_knowledge.md     # YAML frontmatter, entry types, tags
    ├── integration/
    │   ├── adding_mcp_servers.md    # Register external MCPs, discover tools
    │   ├── web_search_and_fetch.md  # Web search + page fetch workflows
    │   └── using_the_registry.md    # Auth, push, pull, publish workflow
    └── orchestration/
        ├── threading_directives.md  # Thread execution, model tiers, limits
        ├── parallel_execution.md    # Parallel thread spawning patterns
        └── safety_and_limits.md     # Permissions, cost limits, safety harness
```

---

## Summary

| Category           | Directive Count | Bundle            |
| ------------------ | --------------- | ----------------- |
| `rye/core/`        | 17              | rye-core + rye-os |
| `rye/primary/`     | 4               | rye-os            |
| `rye/bash/`        | 1               | rye-os            |
| `rye/file-system/` | 6               | rye-os            |
| `rye/web/`         | 2               | rye-os            |
| `rye/lsp/`         | 1               | rye-os            |
| `rye/mcp/`         | 6               | rye-os            |
| `rye/agent/`       | 5               | rye-os            |
| `rye/authoring/`   | 3               | rye-os            |
| `rye/onboarding/`  | 16              | rye-os            |
| **Total**          | **61**          |                   |

---

## Migration Actions

1. **Move** `rye/core/create_directive.md` → `rye/authoring/create_directive.md`
2. **Move** `rye/core/create_tool.md` → `rye/authoring/create_tool.md`
3. **Move** `rye/core/create_knowledge.md` → `rye/authoring/create_knowledge.md`
4. **Move** `rye/core/create_threaded_directive.md` → `rye/agent/create_threaded_directive.md`
5. **Create** all new directives per the map above
6. **Update** category metadata in moved directives to match new paths
