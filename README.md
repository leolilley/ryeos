> **EXPERIMENTAL**: This project is under active development. Features are subject to change.

# RYE OS

> _"In Linux, everything is a file. In RYE, everything is data."_

**The last MCP your agent will ever need.**

Built on **Lilux**, a microkernel providing pure execution primitives, RYE solves the core interoperability failure in AI: **workflows, tools, and context are trapped inside individual projects and agents**. Every environment understands _what_ to do (“scrape a website”), but the _how_ — the steps, tools, and learned knowledge — is locked away, forcing agents to awkwardly read other projects or relearn and rebuild everything from scratch.

RYE breaks that loop. Agents can discover, pull, and execute workflows directly from a shared local or online registry, reuse existing tools without manual setup, and operate across projects without human glue code.

**Your agent becomes self-sufficient.**

---

## The Problem: Fragmentation

AI agents are powerful, but they're trapped. When your agent starts a new project, it has two choices: ask you to copy over workflows, or rebuild everything from scratch. Neither scales.

- **No self-service**: Your agent can't pull the web scraper it used yesterday.
- **No portability**: Workflows that exist in Project A are invisible to Project B.
- **No consistency**: Every project reinvents the same tools because agents can't share.
- **No discoverability**: Your agent rebuilds what already exists because it can't search.

**RYE empowers your agent to solve this itself.** Search the registry. Load workflows. Share tools. Execute. All self managed.

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

  <process>
    <step name="fetch">
      <execute item_type="tool" item_id="web/scraper">
        <param name="url" value="${inputs.target_url}" />
        <param name="selector" value="${inputs.selector}" />
      </execute>
    </step>
  </process>
</directive>
```

### 2. Tools (Executable Code)

Python, JavaScript, YAML, or Bash scripts with metadata headers:

```python
__version__ = "1.0.0"
__executor_id__ = "python_runtime"
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
id: python-async-patterns
category: patterns/async
tags: [python, async]
---

# Python Async Best Practices

## When to Use Async

- I/O-bound operations
- Network requests
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
- Old `rye:validated:` format rejected entirely

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
  "root": {
    "tool_id": "web/scraper",
    "version": "1.0.0",
    "integrity": "sha256:a1b2c3..."
  },
  "resolved_chain": [
    { "tool_id": "web/scraper", "integrity": "sha256:a1b2c3..." },
    { "tool_id": "python_runtime", "integrity": "sha256:d4e5f6..." },
    { "tool_id": "subprocess", "integrity": "sha256:g7h8i9..." }
  ]
}
```

**Why lockfiles matter:**

- **Reproducibility**: Same chain every time
- **Security**: Verify each layer hasn't changed
- **Caching**: Skip resolution if lockfile matches
- **Audit**: Complete execution trace

Lockfiles are stored in `USER_SPACE/.ai/lockfiles/` and committed to version control. When you share your project, others get the exact same tool chain.

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

- **Self-service discovery**: Your agent finds solutions without asking you
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

| Agent Says              | RYE Tool Call                                                                            |
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
    <model tier="haiku" id="claude-3-5-haiku-20241022">Fast scraping with fallback</model>
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

## RYE vs Traditional Agent SDKs

Traditional agent SDKs (like LangChain, OpenAI Assistants, CrewAI) provide:

- **Runtime-driven execution**: Imperative code with hardcoded logic
- **Framework coupling**: Tools only work within that framework
- **Implicit security**: Policy filtering baked into runtime
- **Non-portable**: Workflows don't transfer across environments

**RYE is different.**

| Aspect                  | Traditional SDKs             | RYE                                                |
| ----------------------- | ---------------------------- | -------------------------------------------------- |
| **Workflow Definition** | Code with decorators         | XML data files                                     |
| **Tool Discovery**      | Import and register manually | Agent pulls from registry automatically            |
| **Execution**           | Direct function calls        | Chain-based primitive resolution                   |
| **Security**            | Runtime policy filtering     | Ed25519 signatures + capability tokens             |
| **Portability**         | Locked to framework          | Works in any MCP environment                       |
| **Sharing**             | Package registries           | Agent-accessible cryptographically-signed registry |
| **Extensibility**       | Write code + rebuild         | Agent drops files into `.ai/tools/`                |
| **Agent Autonomy**      | Agent waits for setup        | Agent self-serves workflows                        |

### The Key Difference

**Traditional SDK**: Tools are TypeScript/Python functions registered at runtime. Security is layered policy filtering. Execution is direct.

**RYE**: Tools are data files with metadata headers. Security is Ed25519 signatures for integrity plus capability tokens for permission scoping. Execution builds a layered chain (tool → runtime → primitive) where every element is cryptographically verified before running.

### Why It Matters: Agent Empowerment

**With traditional SDKs**, your agent is dependent on you:

1. You write framework-specific code
2. You manage complex policy configurations
3. You rebuild for every change
4. You manually copy workflows between projects
5. Your agent waits helplessly when it needs new tools

**With RYE**, your agent becomes self-sufficient:

1. **Your agent** discovers workflows in the registry
2. **Your agent** pulls tools it needs on demand
3. **Your agent** shares workflows with your team programmatically
4. **Your agent** uses the same capabilities across Claude Desktop, Cursor, Windsurf, or any MCP client
5. **Your agent** solves its own fragmentation problems

### Deployment: HTTP Server vs. Agent Runtime

**Traditional SDK Deployment:**

Traditional SDKs require embedding their agent runtime into your application:

```python
# LangChain/CrewAI - Runtime coupled with your app
from crewai import Agent, Task, Crew

agent = Agent(
    role='Web Scraping Specialist',
    goal='Extract data from websites',
    tools=[web_scraper_tool]  # Must import/register tools manually
)

task = Task(
    description='Scrape product data from target website',
    agent=agent
)

crew = Crew(agents=[agent], tasks=[task])
result = crew.kickoff()  # Blocking, stateful runtime
```

Problems:

- Runtime bloat in your application
- Hard to scale horizontally
- State management complexity
- Must rebuild/redeploy to update workflows

**RYE Deployment:**

RYE workflows run via **deterministic tool calls**. Wrap the MCP in a simple HTTP server:

```python
# http_server.py - Minimal FastAPI wrapper
from fastapi import FastAPI
from rye.server import rye_mcp_server

app = FastAPI()

@app.post("/api/web/scrape")
async def web_scrape(url: str, selector: str):
    # Spawn a directive thread via MCP—your agent pulls this workflow
    result = await rye_mcp_server.execute(
        item_type="directive",
        item_id="web/scraper",
        inputs={"url": url, "selector": selector}
    )
    return result

@app.post("/api/deploy")
async def deploy(environment: str, version: str):
    result = await rye_mcp_server.execute(
        item_type="directive",
        item_id="deployment/pipeline",
        inputs={"env": environment, "version": version}
    )
    return result
```

**Advantages:**

1. **Stateless**: Each request spawns a fresh directive thread
2. **Scalable**: Horizontal scaling with load balancers
3. **Observable**: Each execution is a deterministic tool call with full traceability
4. **Hot-swappable**: Update `.ai/directives/` files without redeploying
5. **Language-agnostic**: HTTP API works with any client

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

**Traditional SDK**: Runtime is the agent. Stateful. Complex scaling.

**RYE**: Runtime is an HTTP API. Stateless. Simple scaling. Workflows are data.

---

**Note:** RYE is not yet published to PyPI. Install from source:

```bash
git clone https://github.com/leolilley/rye-os.git
cd rye-os/rye
pip install -e .
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

**Cursor, Windsurf, or any MCP client:** Configure MCP server path to `rye`.

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
