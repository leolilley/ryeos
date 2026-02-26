```yaml
id: ryeos-http
title: "ryeos-http — HTTP Server Wrapper"
description: An HTTP server that funnels incoming requests through front-end directives — each directive defines tone, style, permissions, model, and budget for its use case, and every request spawns a thread
category: future
tags: [http, server, api, rest, remote, service, deployment, directives]
version: "0.1.0"
status: exploratory
```

# ryeos-http — HTTP Server Wrapper

> **Status:** Exploratory — architecturally straightforward, not scheduled for implementation.

## The Idea

RYE's transport today is MCP (stdio/SSE via `ryeos-mcp`). `ryeos-http` adds an HTTP transport — but it's not a generic REST API that exposes `search`/`load`/`execute`/`sign` as raw endpoints. That would be `ryeos-cli` over HTTP, and it misses the point.

Instead, `ryeos-http` maps HTTP routes to **front-end directives**. Each front-end directive is a fully configured entry point — it defines the tone, style, permissions, model, budget, and behavior for a specific use case. Every incoming request gets funneled through one of these directives and spawned as a thread.

The server doesn't expose RYE's internals. It exposes a small number of purpose-built directives that happen to be invoked over HTTP.

---

## Front-End Directives

A front-end directive is a standard RYE directive that's designed to be the entry point for external requests. It receives the incoming payload, knows what to do with it, and has all the constraints baked in.

### Example: Discord Bot

```yaml
# .ai/directives/my-bot/discord-respond.md
```

```xml
<directive name="discord-respond" version="1.0.0">
  <metadata>
    <description>Handle an incoming Discord message. Respond in character
    as a helpful but concise assistant. Has access to project knowledge
    and the bash tool for code questions.</description>
    <model tier="fast" />
    <limits max_turns="5" max_tokens="4096" max_spend="0.05" />
    <permissions>
      <execute>
        <tool>rye/bash/bash</tool>
        <tool>rye/file-system/read</tool>
      </execute>
      <load>
        <knowledge>my-bot/knowledge/*</knowledge>
      </load>
    </permissions>
  </metadata>

  <inputs>
    <input name="message" type="string" required="true">
      The user's message content
    </input>
    <input name="username" type="string" required="true">
      The Discord username
    </input>
    <input name="channel" type="string" required="false">
      The channel name for context
    </input>
  </inputs>
</directive>

You are responding to a Discord message from {input:username} in #{input:channel}.

Keep responses under 2000 characters (Discord limit). Be concise, direct,
no preamble. If asked about code, use the bash tool to investigate before
answering. Load relevant knowledge entries if they exist.

Message: {input:message}
```

Everything about how this bot behaves — model, budget, permissions, tone, style — lives in the directive. The HTTP server just funnels requests to it.

### Example: API Service

```xml
<directive name="analyze-pr" version="1.0.0">
  <metadata>
    <description>Analyze a pull request diff and return structured
    feedback. Used by CI pipeline integration.</description>
    <model tier="standard" />
    <limits max_turns="10" max_tokens="8192" max_spend="0.50" />
    <permissions>
      <execute>
        <tool>rye/bash/bash</tool>
        <tool>rye/file-system/*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="diff" type="string" required="true">The PR diff content</input>
    <input name="repo" type="string" required="true">Repository name</input>
    <input name="pr_number" type="integer" required="true">PR number</input>
  </inputs>
</directive>
```

Different model, different budget, different permissions, different style — all declared in the directive, not in HTTP server config.

---

## Route Configuration

The server maps HTTP routes to front-end directives. Configuration is a YAML file:

```yaml
# ryeos-http.yaml
server:
  host: 0.0.0.0
  port: 8080

routes:
  - path: /discord
    method: POST
    directive: my-bot/discord-respond
    # Input mapping: HTTP request fields → directive inputs
    input_map:
      message: body.content
      username: body.author.username
      channel: body.channel_name

  - path: /analyze-pr
    method: POST
    directive: ci/analyze-pr
    input_map:
      diff: body.diff
      repo: body.repository
      pr_number: body.pr_number

  - path: /summarize
    method: POST
    directive: api/summarize-document
    input_map:
      content: body.text
      format: body.output_format
```

Each route is a thin mapping: extract fields from the HTTP request, inject them as directive inputs, thread the directive, return the result. The `input_map` handles the translation from whatever shape the external system sends to whatever shape the directive expects.

### What Happens Per Request

```
POST /discord  {"content": "what does this error mean?", "author": {"username": "leo"}, ...}
    │
    ▼
ryeos-http
  ├── Route match: /discord → my-bot/discord-respond
  ├── Input extraction: body.content → message, body.author.username → username
  └── Thread spawn:
      rye_execute(
          item_type="directive",
          item_id="my-bot/discord-respond",
          parameters={"message": "what does this error mean?", "username": "leo"},
      )
    │
    ▼
Thread runs (LLM loop, tool calls, knowledge loading — all scoped by directive)
    │
    ▼
Response: {"result": "That error means...", "thread_id": "thread_abc123", "spend": 0.03}
```

Every request is a thread. The directive controls everything about how the thread behaves. The HTTP server is just plumbing.

---

## Why Not Expose Raw Primitives

The alternative design — expose `search`, `load`, `execute`, `sign` as REST endpoints and let callers construct arbitrary RYE invocations — is essentially `ryeos-cli` over HTTP. That's the wrong abstraction for a deployed service:

| Raw primitives (ryeos-cli style) | Front-end directives (this proposal) |
| -------------------------------- | ------------------------------------ |
| Caller must know RYE internals   | Caller sends domain-specific payload |
| Permissions managed per-token    | Permissions baked into the directive |
| Model/budget decided by caller   | Model/budget decided by author       |
| Any tool callable                | Only declared tools accessible       |
| Generic API surface              | Purpose-built endpoints              |

A Discord bot shouldn't need to know about `rye_execute` internals. It sends a message and gets a response. The directive author controls what happens in between — model selection, budget, permissions, tone, available tools. The HTTP layer is invisible.

This also means you can audit and version the behavior of each endpoint as a standard RYE item. The directive is signed, overridable at any space level, and its permissions attenuate down the thread hierarchy. Changing how the bot responds is a directive edit, not a server config change.

---

## Architecture

```
HTTP request
    │
    ▼
ryeos-http (ASGI server — route matching, input extraction, auth)
    │
    ▼
execute directive (spawns thread with the matched front-end directive)
    │
    ▼
ryeos (executor, resolver, signing — the full stack)
    │
    ▼
lillux (subprocess, HTTP, signing, integrity primitives)
```

`ryeos-http` imports `ryeos` directly — same as `ryeos-mcp` imports it for MCP transport. The HTTP layer handles routing and input extraction. Everything else is standard RYE execution.

### Package Structure

```bash
pip install ryeos-http
```

Dependencies: `ryeos` (which brings `lillux`) + an ASGI framework. No MCP dependency.

### Deployment

```bash
# Standalone
ryeos-http --config ryeos-http.yaml

# With a process manager
gunicorn ryeos_http:app -w 4 -k uvicorn.workers.UvicornWorker

# Docker
docker run -v /path/to/.ai:/app/.ai -v ryeos-http.yaml:/app/ryeos-http.yaml ryeos-http
```

---

## Authentication

The HTTP layer needs auth since it's network-exposed. But the auth model is simpler than a generic API because the server doesn't expose arbitrary RYE operations — it exposes a fixed set of routes, each bound to a directive that already has its own permission scope.

- **Bearer tokens** — API keys per-client, validated before routing
- **Per-route auth** — different tokens can be authorized for different routes. A Discord webhook token can hit `/discord` but not `/analyze-pr`.
- **No capability mapping needed** — the directive's `<permissions>` block already constrains what the thread can do. The HTTP auth only gates who can trigger the route, not what happens inside.

```yaml
# Route-level auth in ryeos-http.yaml
routes:
  - path: /discord
    method: POST
    directive: my-bot/discord-respond
    auth:
      tokens: ["discord-webhook-token"]
    input_map:
      message: body.content
      username: body.author.username
```

---

## Streaming

Threads can be long-running. Two response modes:

- **Synchronous** (default) — the request blocks until the thread completes, returns the final result. Good for fast directives (< 30s).
- **Async** — returns immediately with a `thread_id`. Caller polls or receives a webhook callback. Good for expensive directives.

```yaml
routes:
  - path: /analyze-pr
    method: POST
    directive: ci/analyze-pr
    async: true # return thread_id immediately
    callback_url: body.callback_url # optional: POST result to this URL when done
```

For sync mode with long-running directives, SSE streaming is an option — the server streams thread events as they happen, matching what `ryeos-mcp` already supports for its SSE transport.

---

## Open Design Questions

### Concurrency

Each request spawns a thread, which uses `os.fork()` for autonomous execution. The server needs to manage concurrent forks — process limits, cleanup, resource exhaustion protection. An ASGI worker model (e.g., uvicorn with worker limits) provides natural backpressure.

### Multi-Project

A single server could serve multiple projects by mapping routes to different `.ai/` directories. Or run one instance per project — simpler isolation, matches how `ryeos-mcp` works today.

### Webhook Integration

Many external systems (Discord, Slack, GitHub) send webhooks with specific payload shapes. The `input_map` in route config handles the field extraction, but webhook verification (signature checking) needs per-platform support. This could be a set of middleware plugins or just handled in the front-end directive itself.

### Health and Observability

A `/health` endpoint for load balancers. Thread-level observability comes free from RYE's existing transcript system — every thread writes to `.ai/agent/threads/{id}/transcript.jsonl`. The HTTP layer could add request-level metrics (latency, status codes, directive hit rates).

---

## Relationship to Existing Infrastructure

| Existing Component                          | How ryeos-http Uses It                                |
| ------------------------------------------- | ----------------------------------------------------- |
| `execute directive`                         | Every request spawns a thread via execute directive   |
| Front-end directives (standard `.md` files) | Define behavior, model, budget, permissions per route |
| Executor chain (tool → runtime → primitive) | Same chain, invoked per-request                       |
| Three-tier spaces (project → user → system) | Same resolution from the server's working directory   |
| Ed25519 signing                             | Directives are signed, thread integrity maintained    |
| Capability attenuation                      | Directive permissions scope each thread               |
| Transcript system                           | Full observability per-request via JSONL logs         |
