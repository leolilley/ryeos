> **EXPERIMENTAL**: This project is under active development. Features may be unstable and are subject to change.

# RYE OS

> _"In Linux, everything is a file. In RYE, everything is data."_

**The last MCP your agent will ever need.**

Built on **Lilux**, a microkernel providing pure execution primitives, RYE (RYE Your Execution) solves the core interoperability failure in AI: **workflows, tools, and context are trapped inside individual projects and agents**. Every environment understands _what_ to do (“scrape a website”), but the _how_ — the steps, tools, and learned knowledge — is locked away, forcing you to point your agent to other projects, copy over workflows and context manually or relearn and rebuild everything from scratch. This doesn't scale.

RYE breaks that loop. Agents can search, load, and execute across tools, workflows and knowledge from a shared local or online registry. Enabling full reuseability without manual setup and clean operations across project contexts.

**Your agent becomes self-sufficient.**

---

## The Problem: Fragmentation

AI agents are powerful, but they're trapped.

- **No self-service**: Your agent can't pull the web scraper it used yesterday.
- **No portability**: Workflows that exist in Project A are invisible to Project B.
- **No consistency**: Every project reinvents the same tools because agents can't share.
- **No discoverability**: Your agent rebuilds what already exists because it can't search.

**RYE empowers your agent to solve this itself through MCP tooling.**

---

## The Primary MCP Tools

RYE exposes just four primary tools that your agent uses to interact with the entire system:

| Tool      | Purpose                                                    | Example                                                       |
| --------- | ---------------------------------------------------------- | ------------------------------------------------------------- |
| `search`  | Discover items across your project, user space, and system | `search(item_type="directive", query="scraping")`             |
| `load`    | Pull content from the registry or local stores             | `load(item_type="tool", item_id="acme/web/scraper")`          |
| `sign`    | Cryptographically sign items with Ed25519                  | `sign(item_type="directive", item_id="web/scraper")`          |
| `execute` | Run directives, tools, or knowledge items                  | `execute(item_type="tool", item_id="web/scraper", url="...")` |

These four primitives compose everything your agent does. Search finds what's available, load pulls it in, sign establishes trust, and execute runs it.

---

## The Physics of AI Prompting

> _"Once you understand the physics, then you can play the game. RYE aims to be the maintainer of those physics."_

Every AI system has the same underlying mechanics:

- Prompts need to be understood
- Tools need to be discovered and executed
- Workflows need to be orchestrated
- Permissions need to be enforced
- Costs need to be tracked
- State needs to be managed

RYE encodes these fundamentals as **data**, not implementation. The physics are consistent. Only the execution environment changes.

---

## Data-Driven Everything

RYE treats three types of items as structured data:

### 1. Directives (XML Workflows in Markdown Files)

Declarative workflows stored as XML-embedded markdown:

```xml
<directive name="web_scraper" version="1.0.0">
  <metadata>
    <description>Extract data from websites</description>
    <category>automation</category>
  </metadata>

  <inputs>
    <param name="target_url" type="string" required="true" />
    <param name="selector" type="string" required="true" />
  </inputs>

  <process>
    <step name="load_knowledge">
      <execute item_type="knowledge" item_id="patterns/scraping/web-scraping-best-practices" />
    </step>
    <step name="fetch">
      <execute item_type="tool" item_id="web/scraper">
        <param name="url" value="${inputs.target_url}" />
        <param name="selector" value="${inputs.selector}" />
      </execute>
    </step>
  </process>

  <outputs>
    <param name="data" type="array" />
  </outputs>
</directive>
```

#### Why XML for Directives?

XML provides:

- **Schema validation** for workflow structure
- **Clear parameter typing** and explicit nesting
- **Standardized parsing** across languages
- **Explicit declarations** that LLMs can generate reliably

While more verbose than YAML, XML's rigidity ensures directives are machine-readable without ambiguity. This matters when your agent is generating and modifying workflows programmatically.

### 2. Tools (Executable Code)

Python, JavaScript, YAML, or Bash scripts with metadata headers:

```python
__version__ = "1.0.0"
__executor_id__ = "python_runtime"
__catergory__ = "web"
__tool_type__ = "automation"

async def main(**kwargs):
    url = kwargs.get('url')
    selector = kwargs.get('selector')
    # Scraping logic
    return {"data": []}
```

### 3. Knowledge (Patterns, Findings, etc)

Structured learnings with YAML frontmatter:

```markdown
---
id: web-scraping-best-practices
category: patterns/scraping
tags: [scraping, css-selectors, rate-limiting]
---

# Web Scraping Best Practices

## CSS Selector Tips

- Use specific class names over generic tags
- Avoid brittle XPath expressions
- Handle dynamic content with wait strategies

## Rate Limiting

- Respect robots.txt
- Add delays between requests
- Use exponential backoff on 429 responses
```

All three live in your project's `.ai/` directory:

```
.ai/
├── directives/     # XML workflows
├── tools/          # Executable scripts
└── knowledge/      # Patterns & learnings
```

### Content Integrity: Ed25519 Signed Everything

Every directive, tool, and knowledge item carries an Ed25519 signature. Unsigned items are rejected — no execution, no loading, no bypass.

```markdown
<!-- rye:signed:2026-02-11T00:00:00Z:a1b2c3d4...:base64url_sig:0a3f9b2c1d4e5f67 -->

# Directive Name
```

**Signature format:** `rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP`

**What each field provides:**

1. **Tamper detection**: SHA256 content hash recomputed and compared on every use
2. **Provenance**: Ed25519 signature binds content to a specific keypair
3. **Trust**: Signing key must exist in the local trust store (`{USER_SPACE}/.ai/trusted_keys/`)
4. **Registry attestation**: Registry re-signs on push, appending `|provider@username`

**How it works:**

- Keypair auto-generated on first sign (`{USER_SPACE}/.ai/keys/`)
- Own public key auto-trusted — self-signed items verify locally with no setup
- Registry public key pinned on first pull (Trust On First Use)
- Every tool in the execution chain is verified before running

---

## The Execution Layer: Lilux Primitives

Every tool in RYE ultimately runs through **two execution primitives** provided by Lilux:

### 1. SubprocessPrimitive

Executes shell commands in isolated environments:

- Process isolation
- Environment variable injection
- Timeout and signal handling
- Output capture (stdout/stderr)

### 2. HttpClientPrimitive

Makes HTTP requests with retry logic:

- Authentication header management
- Timeout and retry configuration
- Response streaming
- Error handling

**That's it.** All tool execution reduces to these two primitives.

### The Tool Chain

RYE builds a recursive chain for every tool execution:

```
Your Tool (e.g., web/scraper.py)
    ↓ __executor_id__ = "python_runtime"
Python Runtime
    ↓ __executor_id__ = "subprocess"
Subprocess Primitive
    ↓ executor_id = None (is primitive)
Execute via Lilux
```

The chain resolves recursively: each layer's `__executor_id__` points to the next layer until reaching a primitive (where `executor_id = None`). Common chain lengths are 2-4 layers, but there's no fixed limit.

Each layer validates before passing down. The chain ensures compatibility between tool requirements and execution environment.

### Lockfiles for Determinism

Every resolved chain generates a lockfile:

```json
{
  "lockfile_version": 1,
  "generated_at": "2026-02-11T00:00:00+00:00",
  "root": {
    "tool_id": "web/scraper",
    "version": "1.0.0",
    "integrity": "a1b2c3d4e5f6...64 hex chars"
  },
  "resolved_chain": [
    {
      "item_id": "web/scraper",
      "space": "project",
      "tool_type": "python",
      "executor_id": "rye/core/runtimes/python_runtime",
      "integrity": "a1b2c3d4...64 hex chars"
    },
    {
      "item_id": "rye/core/runtimes/python_runtime",
      "space": "system",
      "tool_type": "runtime",
      "executor_id": "rye/core/primitives/subprocess",
      "integrity": "e5f6a7b8...64 hex chars"
    },
    {
      "item_id": "rye/core/primitives/subprocess",
      "space": "system",
      "tool_type": "primitive",
      "executor_id": null,
      "integrity": "c9d0e1f2...64 hex chars"
    }
  ]
}
```

**Why lockfiles matter:**

- **Reproducibility**: Same chain every time
- **Security**: Verify each layer hasn't changed
- **Caching**: Skip resolution if lockfile matches
- **Audit**: Complete execution trace

---

## Universal MCP Discovery

RYE doesn't just provide tools—it **absorbs other MCP servers** and turns them into data-driven tools that your agent can discover and use on its own.

**Your agent connects to MCP servers itself.** No manual configuration. No copying endpoints. Your agent discovers tools and starts using them immediately:

**Your agent says:**

```
"discover mcp server https://api.context7.com/mcp"
"list mcp servers"
"execute mcp context7 search for authentication patterns"
```

**Your agent executes these as tool calls:**

| You Say                      | RYE Tool Call                                                               |
| ---------------------------- | --------------------------------------------------------------------------- |
| `discover mcp server X`      | `execute(item_type="tool", item_id="rye/mcp/manager", action="add", url=X)` |
| `list mcp servers`           | `execute(item_type="tool", item_id="rye/mcp/manager", action="list")`       |
| `execute mcp X search for Y` | `execute(item_type="tool", item_id="mcp/X/search", query=Y)`                |

**Example: Mapping "rye discover mcp server https://api.context7.com/mcp"**

The LLM translates this to:

```json
{
  "item_type": "tool",
  "item_id": "rye/mcp/manager",
  "action": "add",
  "name": "context7",
  "transport": "http",
  "url": "https://api.context7.com/mcp"
}
```

**Execution Chain for MCP Tools**

When you execute `mcp/context7/search`, the chain resolves based on transport:

**HTTP Transport:**

```
mcp/context7/search (MCP tool YAML config)
    ↓ __executor_id__ = "rye/core/runtimes/mcp_http_runtime"
rye/core/runtimes/mcp_http_runtime (Layer 2 runtime)
    ↓ __executor_id__ = "rye/core/primitives/subprocess"
rye/core/primitives/subprocess (Layer 1 primitive)
    ↓ executor_id = None
Execute via Lilux
```

**stdio Transport:**

```
mcp/local-script/analyze (MCP tool YAML config)
    ↓ __executor_id__ = "rye/core/runtimes/mcp_stdio_runtime"
rye/core/runtimes/mcp_stdio_runtime (Layer 2 runtime)
    ↓ __executor_id__ = "rye/core/primitives/subprocess"
rye/core/primitives/subprocess (Layer 1 primitive)
    ↓ executor_id = None
Execute via Lilux
```

The MCP runtimes handle the transport-specific connection logic, then delegate to the subprocess primitive for actual execution.

### How It Works

1. **Discovery**: RYE connects to external MCP servers via stdio, HTTP (Streamable), or SSE
2. **Conversion**: Each discovered tool becomes a YAML configuration in `.ai/tools/mcp/`
3. **Data-Driven Execution**: External tools are executed through RYE's chain resolution
4. **Environment Integration**: Auto-loads `.env` files for API keys and configuration

```
.ai/tools/mcp/
├── servers/
│   └── context7.yaml          # Server configuration
└── context7/
    ├── search.yaml            # Discovered tool configs
    ├── resolve.yaml
    └── get-library.yaml
```

### Customize Your Command Language

Want your agent to understand your own phrasing? Add a command dispatch table to your `AGENTS.md`:

```markdown
## COMMAND DISPATCH TABLE

| User Says                    | Run Directive          | With Inputs |
| ---------------------------- | ---------------------- | ----------- |
| `connect to X mcp`           | `rye/mcp/discover`     | `url=X`     |
| `what mcp servers do i have` | `rye/mcp/list_servers` | none        |
| `search X using mcp Y`       | `mcp/Y/search`         | `query=X`   |
```

Now your agent understands _your_ language while still executing RYE directives.

### Universal Compatibility

- **stdio**: Local CLI tools (e.g., custom scripts)
- **HTTP**: Remote services with Streamable HTTP transport
- **SSE**: Legacy SSE transport support

Environment variables are automatically resolved from:

- User space: `USER_SPACE/.env` (default: `~/.ai/.env`)
- Project: `./.ai/.env`, `./.env`, `./.env.local`

This means RYE becomes a **universal MCP client**. One connection point. Every MCP server accessible as data-driven tools.

---

## The Registry: Your Agent's Toolbox

The registry is a centralized, cryptographically-signed store that your agent can access directly:

- **Self-service discovery**: Your agent finds solutions itself
- **On-demand pulling**: Your agent downloads workflows it needs, when it needs them
- **Validation**: Items are Ed25519-signed and verified against a local trust store
- **Sharing**: Push your workflows, pull others—programmatically

Identity model: `namespace/category/name`

The registry is just another data-driven tool your agent can invoke:

**Your agent says:**

```
"Search the registry for web scraper directive"
"Pull acme/web/scraper from registry"
"Push my web/scraper tool to registry"
```

**These become tool calls your agent executes:**

| You Prompt              | RYE Tool Call                                                                            |
| ----------------------- | ---------------------------------------------------------------------------------------- |
| `search registry for X` | `execute(item_type="tool", item_id="rye/registry/registry", action="search", query=X)`   |
| `pull X from registry`  | `execute(item_type="tool", item_id="rye/registry/registry", action="pull", item_id=X)`   |
| `push X to registry`    | `execute(item_type="tool", item_id="rye/registry/registry", action="push", item_path=X)` |

**Example: Your agent searching for a scraper**

```json
{
  "item_type": "tool",
  "item_id": "rye/registry/registry",
  "action": "search",
  "query": "web scraper",
  "item_type": "directive"
}
```

No more waiting for you to set up tools. Your agent helps itself.

Ed25519 signatures for provenance. Server-side validation. Local integrity verification via trust store.

---

## LLM Threads Inside the MCP

RYE doesn't just orchestrate—it **runs agents inside the MCP**.

Spawn isolated LLM threads with scoped permissions:

```xml
<directive name="parallel_scraping" version="1.0.0">
  <metadata>
    <description>Run web scraping in parallel using spawned threads</description>
    <category>automation</category>
    <author>rye</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits max_turns="8" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.agent.threads.spawn_thread</tool>
        <tool>rye.file-system.fs_read</tool>
      </execute>
      <search>
        <directive>web/*</directive>
      </search>
      <load>
        <directive>*</directive>
      </load>
    </permissions>
    <cost>
      <context estimated_usage="medium" turns="8" spawn_threshold="3">
        4096
      </context>
      <duration>300</duration>
      <spend currency="USD">5.00</spend>
    </cost>
    <hooks>
      <hook>
        <when>cost.current > cost.limit * 0.8</when>
        <execute item_type="directive">notify-cost-threshold</execute>
      </hook>
      <hook>
        <when>error.type == "permission_denied"</when>
        <execute item_type="directive">request-elevated-permissions</execute>
        <inputs>
          <requested_resource>${error.resource}</requested_resource>
        </inputs>
      </hook>
    </hooks>
  </metadata>

  <process>
    <step name="spawn_scraper">
      <execute item_type="tool" item_id="rye.agent.threads.spawn_thread">
        <param name="thread_id" value="scraping-worker" />
        <param name="directive_name" value="web/scraper" />
      </execute>
    </step>
  </process>
</directive>
```

### Safety Harness

Every thread runs with:

- **Cost tracking**: tokens, turns, duration, spend
- **Permission enforcement**: CapabilityToken validation
- **Hook-based error handling**: retry, skip, fail, abort
- **Checkpoint control**: pause, resume, inspect

### Hooks: Conditional Actions

Hooks let you respond to events during execution:

```xml
<hooks>
  <hook>
    <when>cost.current > cost.limit * 0.8</when>
    <execute item_type="directive">notify-cost-threshold</execute>
  </hook>
  <hook>
    <when>error.type == "permission_denied"</when>
    <execute item_type="directive">request-elevated-permissions</execute>
  </hook>
</hooks>
```

Available context: `cost.current`, `cost.limit`, `error.type`, `loop_count`, `directive.name`

---

## Security: Capability-Based Permissions

RYE uses a unified capability system where permissions declared in directives become runtime capability tokens.

### Declaring Permissions

Permissions are declared hierarchically in directive XML:

```xml
<permissions>
  <execute>
    <tool>rye.file-system.fs_read</tool>
    <tool>web/*</tool>
  </execute>
  <search>
    <directive>automation/*</directive>
  </search>
  <load>
    <tool>rye.shell.*</tool>
  </load>
</permissions>
```

When a directive runs, its permissions are converted to capability strings:

- `rye.execute.tool.rye.file-system.fs_read`
- `rye.execute.tool.web.*`
- `rye.search.directive.automation.*`
- `rye.load.tool.rye.shell.*`

### Runtime Enforcement

These capabilities become a **CapabilityToken** for the thread:

```python
# Token created from directive permissions
cap_token = CapabilityToken(
    capabilities={
        "rye.execute.tool.rye.file-system.fs_read",
        "rye.execute.tool.web.*",
        "rye.search.directive.automation.*"
    },
    thread_id="scraping-worker"
)

# Every tool call validates against the token
# Violations raise PermissionDenied
```

### Thread Isolation

Each spawned agent gets its own capability token derived from its directive's permissions, plus:

- Cost budget (tokens, turns, duration, spend)
- Resource limits
- Scoped capability access

### Integrity Verification

Ed25519 signatures are enforced at every boundary:

- Directive verified before thread execution
- Every tool in the execution chain verified before running
- Registry items verified against TOFU-pinned registry key on pull
- Unsigned or tampered items rejected with `IntegrityError`

---

## RYE and Traditional Agent SDKs

**Important**: RYE is not a replacement for LangChain, OpenAI Assistants, or CrewAI (although it can and I believe it will). It's a complementary layer that solves a specific problem those SDKs aren't designed for.

### Honest Comparison

| Aspect              | Traditional SDKs                    | RYE                                        | Trade-off                             |
| ------------------- | ----------------------------------- | ------------------------------------------ | ------------------------------------- |
| **Maturity**        | Battle-tested, 2+ years production  | Experimental, active development           | Stability vs Innovation               |
| **Ecosystem**       | 2000+ integrations, rich community  | MCP-based, emerging                        | Breadth vs Protocol focus             |
| **Discovery Model** | Package registries (human searches) | Agent-searchable registry                  | Manual import vs Autonomous discovery |
| **Portability**     | Python/JS runtime                   | Any MCP client                             | Language lock-in vs Protocol lock-in  |
| **Security Model**  | Runtime policies + package signing  | Ed25519 + capabilities                     | Implicit vs Explicit                  |
| **Agent Autonomy**  | Human configures tools              | Agent discovers & loads                    | Manual vs Autonomous                  |
| **Observability**   | LangSmith, W&B, mature tooling      | Deterministic tool calls, traceable chains | Rich dashboards vs Data transparency  |

### What Each Does Best

**Traditional SDKs excel at:**

- **Building agent applications** with mature frameworks
- **Integration breadth** - thousands of pre-built connectors
- **Developer experience** - debugging tools, IDE support, type safety
- **Production hardening** - retry logic, rate limiting, error recovery
- **Community support** - Stack Overflow, tutorials, consultants

**RYE excels at:**

- **Sharing agent capabilities** across projects and environments
- **Agent-native discovery** - workflows searchable and loadable by agents
- **Cross-environment portability** - same directives in Claude Desktop, Cursor, or any MCP client
- **Explicit provenance** - cryptographic signatures for trust without central authority
- **Agent autonomy** - your agent pulls what it needs without manual setup

### Specific Use Cases

**Traditional SDKs are great when:**

- Building a chatbot with LangSmith observability and production monitoring
- Integrating with 50+ data sources using pre-built LangChain connectors
- You need TypeScript type safety for your React frontend
- You're deploying to production and need battle-tested reliability

**RYE is great when:**

- You're building workflows across Opencode, Amp, Claude Code, Cursor, etc.
- Your agent needs to self-serve tools without you manually copying them between projects
- You want cryptographic proof of workflow provenance
- You're working in MCP-native environments
- You need the same capabilities available regardless of which AI tool you're using

### The Real Problem RYE Solves

**Real scenario:** You build a web scraper in Opencode for Project A. Next week, you start Project B in Claude Code. Your agent can't access the scraper from Project A—it's trapped in that project's code. You copy-paste the code, adjust imports, debug environment differences. A month later, you update the scraper in Project A. Now Project B is out of sync.

Traditional SDKs handle this with package managers, which work great—if a human is doing the importing. But your agent can't `pip install` the scraper it wrote last week. It can't search npm for "that authentication pattern we used before." It rebuilds from scratch every time.

RYE treats workflows as **discoverable, cryptographically-signed data that agents can search, pull, and execute** without human intervention.

### Deployment Options

Both traditional SDKs and RYE support stateless HTTP deployment:

**LangChain with LangServe:**

```python
from fastapi import FastAPI
from langserve import add_routes

app = FastAPI()
add_routes(app, chain, path="/scrape")  # Stateless HTTP endpoint
```

**RYE with MCP wrapper:**

```python
from fastapi import FastAPI
from rye.server import rye_mcp_server

app = FastAPI()

@app.post("/api/scrape")
async def scrape(url: str, selector: str):
    # Create an LLM thread to execute the directive
    return await rye_mcp_server.execute(
        item_type="tool",
        item_id="rye/agent/threads/thread_directive",
        directive_name="web/scraper",
        inputs={"target_url": url, "selector": selector}
    )
```

Both are:

- Stateless ✓
- Scalable ✓
- Observable ✓
- Hot-swappable: LangChain (with dynamic loading), RYE (via registry) ✓

The difference is in workflow management: LangChain workflows live in code repositories. RYE workflows live in a searchable, agent-accessible registry.

**Example: Kubernetes Deployment**

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rye-api
spec:
  replicas: 3
  template:
    spec:
      containers:
        - name: rye-api
          image: myapp/rye-api:latest
          volumeMounts:
            - name: directives
              mountPath: /app/.ai/directives
      volumes:
        - name: directives
          configMap:
            name: workflow-directives
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: workflow-directives
data:
  web/scraper.md: |
    <!-- directive content -->
  deployment/pipeline.md: |
    <!-- directive content -->
```

Update workflows by updating the ConfigMap. No container rebuilds. No downtime.

> **Deployment Difference**: Traditional SDKs package workflows with the application (update via redeployment). RYE keeps workflows as external data (update via ConfigMap/registry without rebuilding containers).

---

## Installation

**Note:** RYE is not yet published to PyPI. Install from source:

```bash
git clone https://github.com/leolilley/rye-os.git
cd rye-os
pip install -e lilux
pip install -e rye
```

### Connect to Your Agent

**Opencode (`.opencode/mcp.json`):**

```json
{
  "mcpServers": {
    "rye": {
      "type": "local",
      "command": ["/path/to/rye"],
      "environment": {
        "USER_SPACE": "~/.ai"
      },
      "enabled": true
    }
  }
}
```

**Amp, Claude Code, Cursor, or any MCP client:** Configure MCP server path to `rye`.

---

## The Fulcrum

> _"Give me a lever long enough and a fulcrum upon which to place it and I shall move the earth."_ — Archimedes

If AI is the lever, this is the fulcrum.

RYE doesn't just run tools—it captures the fundamental physics of AI execution as data. Once you encode workflows, tools, and knowledge as structured data, they become:

- **Autonomous**: Agents self-serve workflows from the registry
- **Portable**: Works in any MCP environment
- **Composable**: Chain directives together
- **Verifiable**: Ed25519 signatures with trust store
- **Shareable**: Registry-based distribution
- **Secure**: Capability-based permissions

This is the future of AI architecture. Once you see it, you can't go back.

---

## License

MIT
