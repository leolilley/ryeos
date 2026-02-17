# RYE OS v1.0 - Strip Back to Core Plan

## Vision Reset

**Original vision:** Data-driven multi-agent orchestration inside MCP with recursive execution chains, capability tokens, and agent spawning.

**Reality check:** This is a laser to swat a fly. The world just needs a good fly swatter.

**New vision for v1.0:**

> A portable environment where users can easily share and run their tools, directives, and knowledge between all their projects, platforms (Cursor, Claude Desktop, Windsurf, etc.), and computers (local, cloud).

## Core Value Proposition

Users want:

1. **Write a tool once, run it anywhere** - Move tools between projects with zero friction
2. **Share what you want, keep what you don't** - Public registry as optional bonus, not primary feature
3. **Simple primitives that work** - Python and Node runtimes that just execute real scripts
4. **No platform lock-in** - Works across all MCP clients (Claude Desktop, Cursor, Windsurf, etc.)

## Breaking Changes (v1.0)

**No backwards compatibility required** - This is a reset for v1.0.

1. **Tool Schema Change** - All tools **must** include `__tool_description__`
   - Existing tools without descriptions will fail validation
   - Required before v1.0 release
2. **Directive Schema Change** - Model, permissions, cost fields now optional
   - Existing directives with these fields still work (just ignored)
   - New minimal directives omit them entirely

3. **Directory Reorganization**
   - Old import paths will break
   - All internal references must be updated

## Migration Notes

- **No automated migration provided** - v1.0 is a clean slate
- Users must manually update existing tools to add `__tool_description__`
- Example migration command (for users):
  ```bash
  rye tools list | xargs -I {} rye tools sign {} --add-description
  ```

## Changes Required

### 1. Remove (Not v1)

**Delete these directories entirely:**

- `rye/rye/.ai/tools/rye/rag/` - RAG system, not needed yet
- `rye/rye/.ai/tools/rye/agent/` - Agent orchestration, not needed yet

**What goes with them:**

- agent/capabilities/ - capability tokens and scopes
- agent/harness/ - safety harness for recursive execution
- agent/llm/ - LLM configuration and pricing
- agent/threads/ - thread spawning and management
- All agent-related directives (pause_thread, resume_thread, spawn_thread, etc.)

### 2. Reorganize (Move to Core)

**Move these tools into `rye/rye/.ai/tools/rye/core/`:**

```
rye/rye/.ai/tools/rye/
 ├── core/
 │   ├── telemetry/          # Move from root level
 │   │   ├── export.py
 │   │   ├── status.py
 │   │   ├── configure.py
 │   │   ├── clear.py
 │   │   └── run_with.py
 │   ├── protocol/           # Keep as-is (JSON-RPC protocol tool)
 │   │   └── jsonrpc_handler.py
 │   ├── mcp/               # Move from rye/.ai/tools/rye/mcp/ (MCP client tools)
 │   │   ├── connect.py
 │   │   └── discover.py
 │   └── registry/           # Move from root level
 │       └── registry.py  # Single comprehensive registry tool (auth, push, pull, keys)
```

**Note:**

- `protocol/` is a generic JSON-RPC protocol tool - keep as-is
- `mcp/` contains MCP client tools (connect, discover) - move into `core/mcp/`
- Registry is already a single comprehensive file - no need to split it

### 3.1. Schema - Already Implements Versioning

**Schema already supports versioning** (`docs/schema/002_functions.sql`):

- **Tables:**
  - `tool_versions` - Multiple versions per tool
  - `directive_versions` - Multiple versions per directive
  - `knowledge_versions` - Multiple versions per knowledge entry
  - `is_latest` flag on each version row

- **Functions:**
  - `is_valid_semver(version)` - Validates semantic version format
  - Search returns `latest_version` field
  - Version filtering via `version` parameter
  - `ORDER BY version DESC` to get most recent

- **Pull behavior:**
  - Default: Pull latest (`is_latest = true`)
  - Explicit: Pull specific version via `--version=1.2.0`
  - Always pulls most recent if not specified

**Note:** Registry is already a single comprehensive file (`registry.py`) - no need to split it.

### 3.2. Signature Format - Inline Comments

**Signatures are embedded as inline comments** (same file, no separate `.sig` file):

**Local signature (generic):**

```python
# rye:validated:2026-02-03T05:00:00Z:abc123def456...
def my_function():
    pass
```

**Registry signature (with username):**

```python
# rye:validated:2026-02-03T05:00:00Z:abc123def456...|registry@leo
def my_function():
    pass
```

**Formats:**

- Local: `rye:validated:timestamp:hash`
- Registry: `rye:validated:timestamp:hash|registry@username`

**Implementation:** Uses `rye/rye/utils/metadata_manager.py`:

- `DirectiveMetadataStrategy` - HTML comment format (`<!-- rye:validated:... -->`)
- `ToolMetadataStrategy` - Language-specific comment format (Python: `# rye:validated:...`)
- `KnowledgeMetadataStrategy` - HTML comment format (`<!-- rye:validated:... -->`)

**Minor MetadataManager update needed:** Extend regex to parse optional `|registry@username` suffix.

**Design principle:** Server-side signing. The `|registry@username` suffix is **only added by the Registry API server** after validating content and authenticating the user. This prevents forged provenance claims.

See [/docs/db/services/registry-api.md](../../../db/services/registry-api.md) for full server-side validation documentation.

**Changes needed in `registry.py` (`_push()`):**

The client push function should:
1. Validate and sign locally (same as `sign` tool) - adds `rye:validated:timestamp:hash`
2. Push to Registry API server
3. Server re-validates using same `rye` validators, adds `|registry@username`
4. Update local file with registry-signed content

```python
async def _push(item_type: str, item_id: str, ...):
    # 1. Load content
    content = path.read_text()
    
    # 2. Validate locally (same as sign tool)
    parsed = parser_router.parse(parser_type, content)
    validation = validate_parsed_data(item_type, parsed, file_path, ...)
    if not validation["valid"]:
        return {"status": "error", "issues": validation["issues"]}
    
    # 3. Sign locally (standard signature, NO registry suffix)
    signed_content = MetadataManager.sign_content(item_type, content, ...)
    
    # 4. Push to Registry API (server will re-validate and add |registry@username)
    response = await http.post(
        f"{registry_url}/v1/push",
        json={
            "item_type": item_type,
            "item_id": item_id,
            "content": signed_content,
            "version": version,
        },
        auth_token=token,
    )
    
    if response.get("status") == "error":
        return response  # Server-side validation failed
    
    # 5. Update local file with registry-signed version
    if response.get("signed_content"):
        path.write_text(response["signed_content"])
    
    return response
```

**Changes needed in `registry.py` (`_pull()`):**

Verify the registry signature on pull:

```python
async def _pull(item_type: str, item_id: str, ...):
    response = await http.get(f"{registry_url}/v1/pull/{item_type}/{item_id}")
    
    content = response["content"]
    author = response["author"]
    
    # Extract and verify signature
    sig_info = MetadataManager.extract_signature(item_type, content)
    if not sig_info:
        return {"status": "error", "message": "No signature found"}
    
    # Verify hash
    content_without_sig = MetadataManager.remove_signature(item_type, content)
    computed_hash = hashlib.sha256(content_without_sig.encode()).hexdigest()
    if computed_hash != sig_info["hash"]:
        return {"status": "error", "message": "Content integrity check failed"}
    
    # Verify username matches author from registry
    if sig_info.get("registry_username") and sig_info["registry_username"] != author:
        return {"status": "error", "message": f"Username mismatch: signature says {sig_info['registry_username']}, registry says {author}"}
    
    dest.write_text(content)
    return {"status": "pulled", ...}
```

**Changes needed in `metadata_manager.py`:**

Extend `extract_signature()` regex to parse optional `|registry@username` suffix:

```python
# Current: r"validated:(.*?):([a-f0-9]{64})"
# New:     r"validated:(.*?):([a-f0-9]{64})(?:\|registry@(\w+))?"

# Returns: {"timestamp": ..., "hash": ..., "registry_username": ... or None}
```

**What stays the same:**

- Local `sign` tool uses simple format (`timestamp:hash`)
- Local sign overwrites registry signature (user modified file = their signature now)
- Both formats are valid, MetadataManager parses either
- Client never adds `|registry@username` - only the server does this

### 3.3. Schema Changes

#### Directive Schema - Make Agent Fields Optional

Update `/docs/rye/primary-items/directive-metadata.md` to make these fields optional:

**Required fields (minimal v1):**

- `name`, `version` (root attributes)
- `description`, `category`, `author` (metadata)

**Optional fields (advanced use, none of these can be enforced without rye agent harness):**

- `model` (with `tier` attribute) - Model specification for execution
- `permissions` - Permission declarations
- `cost` - Cost tracking and budgets
- `context` (relationships, related_files, dependencies)
- `hooks` - Event-driven actions

**Rationale:**

- None of them can actually be enforced without directives running on the rye agent safety harness.
- Simple workflows don't need model tiers, permissions, or cost tracking.
- Power users can add these when needed
- Lowers barrier to entry for v1

**Example (minimal):**

```xml
<directive name="deploy-staging" version="1.0.0">
  <metadata>
    <description>Deploy application to staging environment</description>
    <category>workflows</category>
    <author>devops-team</author>
  </metadata>
  <!-- No model, permissions, or cost needed -->
</directive>
```

**Example (advanced):**

```xml
<directive name="deploy-production" version="1.0.0">
  <metadata>
    <description>Deploy to production with full validation</description>
    <category>workflows</category>
    <author>devops-team</author>

    <model tier="orchestrator" fallback="general" />

    <permissions>
      <execute resource="shell" action="kubectl" />
      <read resource="filesystem" path="k8s/**" />
    </permissions>

    <cost>
      <context estimated_usage="high" turns="20">10000</context>
      <duration>600</duration>
    </cost>
  </metadata>
</directive>
```

### 3.3.1. Directive Tool Refactoring

**Create two separate directives:**

1. **`create_directive`** (NEW) - Simple, minimal directive creator
   - Only asks for required fields: `name`, `version`, `description`, `category`, `author`
   - No model tier, no permissions, no cost tracking
   - Focus: "Just give me a workflow that works"

2. **`create_advanced_directive`** (RENAME from existing `create_directive`)
   - Full progressive disclosure: all fields (model, permissions, cost, context, hooks)
   - Power users get full control
   - Existing implementation already handles this

**Execution:**

- Rename existing `create_directive` → `create_advanced_directive`
- Create new minimal `create_directive` that only prompts for required fields
- Update `create_directive` documentation to reference `create_advanced_directive` for power users

#### Tool Schema - Add Required Description

Current scripts may not have descriptions.

**Change to require:**

```python
"""
Tool description (required)

This tool does X, Y, Z.
"""

__tool_type__ = "runtime"  # or whatever type
__tool_version__ = "1.0.0"
__tool_description__ = "What this tool does (required)"  # NEW
```

### 3.2.1. Signing and Registry Architecture

**Separation of Concerns:**

1. **Primary Code (Generic Signing Only)**
   - `tools/sign.py` - Generic signing tool
   - Signature payload: `{content_hash, timestamp}`
   - No registry awareness
   - Used for personal validation, project-to-project sharing
   - LLM calls this via MCP with params

2. **Registry Tools (Registry-Specific Signing)**
   - `core/registry/push.py` - Push to registry with auto-signing
   - Signature payload: `{content_hash, timestamp, registry@account}`
   - Handles registry authentication
   - Auto-signs with authenticated user's account
   - LLM calls this via MCP with params

**Signature Format (Inline Comments - Same Format, Extra Fields):**

Signatures are embedded as inline comments in the file itself (no separate `.sig` file).

**Generic signature (offline/personal use):**

```python
# rye:validated:2026-02-03T05:00:00Z:abc123def456...
def my_function():
    pass
```

**Key Design Decisions:**

- **Registry signs with username** - `timestamp:hash|registry@username` embeds provenance
- **Local sign is simple** - `timestamp:hash` (no username)
- **Local sign overwrites** - User modifies file → their signature, registry signature gone
- **MetadataManager parses both** - Minor regex update to handle optional suffix
- **Verification checks username** - Pull verifies signature username matches DB author

**MCP Tool Calls (LLM usage):**

```python
# LLM calls local sign
await mcp__rye__execute(
    item_type="tool",
    item_id="sign",
    parameters={
        "path": "/path/to/tool.py"
    }
)
→ Creates signature: rye:validated:timestamp:hash

# LLM calls registry push
await mcp__rye__execute(
    item_type="tool",
    item_id="registry/push",
    parameters={
        "path": "/path/to/tool.py"
    }
)
→ Gets username from auth token
→ Signs with registry format: rye:validated:timestamp:hash|registry@leo
→ Updates local file with registry signature
→ Pushes content to registry
```

**Registry Pull - Verify Signature:**

1. Fetch content from registry (includes inline signature with username)
2. Verify `hash(content_without_sig) == signature_hash`
3. Verify `signature_username == registry_author`
4. If either mismatch → error
5. Write verified content to local file

**Re-signing Flow:**

1. User pulls tool with signature: `rye:validated:...|registry@leo`
2. User modifies the file
3. User runs local `sign`
4. Overwrites with: `rye:validated:...` (no username)
5. File is now theirs, not tied to registry account

### 3.4. Registry Configuration

**Registry URL** - Data-driven, multiple registries supported:

```python
# Environment variable (default: Rye public registry)
RYE_REGISTRY_URL = os.environ.get("RYE_REGISTRY_URL", "https://rye-registry.supabase.co")
```

**Authentication** - Already implemented:

- **Device Auth Flow** (like Supabase CLI):
  - ECDH keypair generation for secure device auth
  - Browser-based OAuth (GitHub, email/password)
  - Encrypted token exchange
  - Token storage in kernel keyring (via `lilux.utils.path_service`)

- **Actions implemented:**
  - `signup` - Create account with email/password
  - `login` - Start device auth flow (opens browser)
  - `login_poll` - Poll for auth completion
  - `logout` - Clear local auth session
  - `whoami` - Show current authenticated user

- **CI/Headless support:**
  - `RYE_REGISTRY_TOKEN` env var for CI workflows
  - Checked before keyring access

### 4. What Stays (The Core)

**These are the essentials that work:**

✅ **4 MCP Tools**

- `mcp__rye__search` - Find items by query
- `mcp__rye__load` - Load content or copy between spaces
- `mcp__rye__execute` - Run directives, tools, knowledge
- `mcp__rye__sign` - Validate and sign items

✅ **3 Item Types**

- Directives - Workflow definitions
- Tools - Executable scripts (Python, Node, etc.)
- Knowledge - Structured information

✅ **Core Tools**

- `core/primitives/` - subprocess, http_client, errors
- `core/runtimes/` - python_runtime, node_runtime
- `core/parsers/` - markdown_frontmatter, markdown_xml, python_ast, yaml
- `core/extractors/` - directive/, knowledge/, tool/
- `core/sinks/` - output sinks
- `core/system/` - system utilities

✅ **Registry**

- Authentication
- Search
- Pull
- Push
- Publish
- Version management

✅ **Telemetry**

- Export logs
- Status checks
- Configuration
- Run with telemetry

### 5. Documentation Updates

**Update README.md:**

Current: "The operating system for artificial intelligence" + lots of multi-agent talk

New focus:

- "Portable AI tools across your projects"
- "Write once, run anywhere"
- "Share what you want, keep what you don't"
- Mention agent/orchestration as "future capability" - not core

**Update docs/index.md:**

- Remove agent orchestration sections
- Focus on portability and tool sharing
- Keep registry as "optional bonus feature"

**Add hint at future:**

- "Built on a data-driven architecture that enables powerful multi-agent orchestration when you're ready for it"

## Testing Checklist

### Core Functionality

- [ ] **MCP Discovery**
  - [ ] Search finds directives across project/user/system spaces
  - [ ] Search finds tools across all categories
  - [ ] Search finds knowledge entries
  - [ ] Load pulls items correctly
  - [ ] Load copies items between project/user spaces

 - [x] **Python Runtime**
   - [x] Execute simple Python script
   - [x] Execute Python script with parameters
   - [x] Execute Python script with environment variables
   - [x] Handle Python script errors gracefully
   - [x] Validate Python tool metadata extraction

 - [x] **Node Runtime**
   - [x] Execute simple Node.js script
   - [x] Execute Node script with parameters
   - [x] Execute Node script with environment variables
   - [x] Handle Node script errors gracefully
   - [x] Validate Node tool metadata extraction

- [ ] **Registry**
  - [ ] Device auth flow (login + login_poll)
  - [ ] Registry search works
  - [ ] Registry pull with signature verification
  - [ ] Registry push with auto-signing
  - [ ] Set visibility (public/private/unlisted)
  - [ ] Key management (generate/list/trust/revoke)
  - [ ] Version resolution (latest vs specific)

- [ ] **Sign/Verify**
  - [ ] Sign a tool
  - [ ] Verify tool signature
  - [ ] Sign a directive
  - [ ] Verify directive signature
  - [ ] Handle invalid signatures
  - [ ] Local sign overwrites registry signature

- [ ] **End-to-End Workflows**
  - [ ] Create tool → Sign → Push to registry → Pull to another project → Execute
  - [ ] Create directive → Load in project → Execute
  - [ ] Share tool between projects via copy → Verify it works
  - [ ] Search registry → Find tool → Pull → Execute

### Regression Tests

- [ ] All existing Lilux tests pass
- [ ] All existing RYE tests pass
- [ ] No broken imports after reorganization

## Ship Criteria

✅ **Must Have:**

1. All removals completed (rag, agent)
2. All reorganizations completed (telemetry, mcp, registry to core)
3. Directive refactoring complete (create_directive split into two versions)
4. Directive metadata updated (model, permissions, cost made optional)
5. Tool schema updated (`__tool_description__` required)
6. All imports fixed after reorganization
7. All existing tests pass (Lilux + RYE)
8. Python runtime working with real scripts (params, env vars, error handling)
9. Node runtime working with real scripts (params, env vars, error handling)
10. Registry auth working (device flow + env var support)
11. Registry push with auto-signing (signature format: timestamp:hash|registry@account)
12. Registry pull with signature verification (3-layer check)
13. Registry search working (with filtering)
14. Registry visibility management working
15. Registry key management working (generate/list/trust/revoke)
16. Version resolution working (latest vs specific version)
17. Documentation updated to reflect v1 focus (portability + tool sharing)

✅ **Nice to Have:**

- Example tools in registry for users to try
- Quick start guide with common workflows
- Video demo (for blog post)

## Execution Order

1. **Create plan** (this document)
2. **Remove rag/agent** - Delete directories
3. **Reorganize** - Move telemetry/mcp/registry to core
4. **Directive refactoring** - Rename `create_directive` → `create_advanced_directive`, create new minimal `create_directive`
5. **Update directive metadata** - Make model, permissions, cost optional
6. **Add tool description** - Add required `__tool_description__` to tool schema
7. **Fix imports** - Update all import statements
8. **Test core** - Run all tests, fix any breakage
9. **Test Python runtime** - Create and test real scripts
10. **Test Node runtime** - Create and test real scripts
11. **Test registry** - Full auth/search/pull/push/publish/key management cycle
12. **Update docs** - Reflect v1 focus
13. **Ship** - Cut release

## Post-Ship

- Share with early users
- Focus on feedback and usability
- Make money (consulting, workshops, custom tools)
- Keep agent/orchestration architecture intact - document it as "future"
- When users ask for "more power," the system is already ready

---

**Remember:** This is a fly swatter. But it's a really good one. And the laser is still there under the hood when they're ready for it.
