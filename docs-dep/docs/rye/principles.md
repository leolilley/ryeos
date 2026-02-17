**Source:** RYE principles from kiwi-mcp data-driven architecture

# RYE Operating System Principles

## What is RYE?

RYE is an **operating system layer** that runs on top of the Lilux microkernel. It provides intelligent tool execution and content understanding.

## Microkernel + OS Relationship

| Aspect              | Lilux (Microkernel)          | RYE (OS)                                |
| ------------------- | ---------------------------- | --------------------------------------- |
| **What**            | Generic execution primitives | 5 MCP tools + executor + data-driven items |
| **Analogy**         | Hardware microkernel         | Operating system                        |
| **Intelligence**    | Dumb - just executes         | Smart - understands content shapes      |
| **Package**         | `lilux`                      | `rye-lilux`                             |
| **Entry Point**     | Not used (dependency)        | `python -m rye.server`                  |
| **MCP Exposure**    | None                         | 5 tools: search, load, execute, sign, help |

## Key Principles

### 1. Data-Driven Architecture

- All tools defined as data in `.ai/tools/`
- No hardcoded tool lists
- On-demand loading from filesystem

### 2. Category Organization

- Tools organized by function and location in three-space model
- Project space: Project-specific tools (highest precedence)
- User space: User's custom tools (medium precedence)
- System space: Bundled RYE tools in site-packages (immutable, lowest precedence)
- Everything is a tool (primitives, runtimes, capabilities, etc.)
- Tools can depend on tools from equal or higher precedence spaces

### 3. Schema/Code Separation

- **Schemas** in `.ai/tools/rye/primitives/` (metadata only)
- **Code** in `lilux/primitives/` (implementation)
- Clear separation of concerns

### 4. Universal Execution

- Single executor routes all tools
- Routes based on `__tool_type__` and `__executor_id__`
- Supports primitives → runtimes → tools chain

## Bottom Line

- User installs RYE → gets OS + microkernel
- LLM sees 5 MCP tools (search, load, execute, sign, help)
- LLM accesses `.ai/` items on-demand via these 5 tools
- RYE executor routes to Lilux primitives for execution
- All intelligence is data-defined in .ai/ (not hardcoded)

## Tool Spaces Model

RYE uses a three-space model for tool resolution with explicit precedence:

| Space    | Location                              | Mutability | Precedence |
| --------- | ------------------------------------- | ------------ | ----------- |
| **Project** | `{project}/.ai/`                   | Mutable     | 1 (highest)  |
| **User**    | `~/.ai/`                            | Mutable     | 2 (medium)   |
| **System**  | `site-packages/rye/.ai/`          | Immutable   | 3 (lowest)   |

**Key Principles:**
- Resolution is deterministic: always searches project → user → system
- Project/user tools can shadow system tools (intentional - enables customization)
- System tools are immutable (read-only, installed via pip)
- Chains can span multiple spaces with explicit validation

**See Also:** [[executor/tool-resolution-and-validation.md]] for complete details on cross-space chain validation.

---

## Architecture Comparison

| Aspect | Old (Hardcoded) | New (Data-Driven) |
|--------|-----------------|-------------------|
| Tool Definition | Hardcoded Python tools | Data files with metadata |
| Item Access | Manual imports | On-demand via 5 MCP tools |
| Tool Language | Python only | Any language (if runtime exists) |
| Tool Behavior | Code-based | Configuration-based (ENV_CONFIG) |
| Extensibility | Code changes required | Add files → tools available |
| Runtime Support | Embedded in tools | Declarative via `.ai/tools/runtimes/` |
| Primitives | Code + schemas mixed | Schemas in `.ai/`, code in `lilux/` |
| Execution Model | Fixed tool set | Dynamic routing via metadata |

---

## On-Demand Loading Model

RYE's on-demand loading is a key departure from traditional MCP servers:

| Aspect | Old (Wrong) | New (Correct) |
|---------|--------------|----------------|
| **Startup** | Scan all tools, build registry | Expose 5 MCP tools |
| **LLM sees** | All individual tools | Only 5 tools |
| **Tool access** | Direct MCP calls | Via `item_type="tool"` |
| **Metadata parsing** | At startup | On-demand |
| **Executor resolution** | At startup | At execution time |

### Benefits of On-Demand Loading

| Benefit | Description |
|----------|-------------|
| **Fast startup** | No scanning or parsing at server start |
| **Unified API** | Same 5 tools for all item types |
| **Simple LLM context** | LLM only needs to know 5 tools |
| **Lazy loading** | Only parse what's actually used |
| **Extensibility** | Add tools by creating files, no registration |
| **Consistent model** | Tools = Directives = Knowledge |

