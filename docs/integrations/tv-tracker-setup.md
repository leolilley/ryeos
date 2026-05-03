# TV Tracker — Rye OS Integration Setup Guide

End-to-end guide for integrating a TV ratings tracker app with Rye OS.
The backend (Express) proxies SSE streams from the daemon to the browser.

---

## Architecture

```
Browser (EventSource)
  │ POST /api/ai/chat { message }
  │ Accept: text/event-stream
  ▼
Backend (Express :4000)
  │ 1. Pre-fetch DB state from ClickHouse (parallel)
  │ 2. Build db_context text (~50K chars)
  │ 3. Sign request, POST to ryeosd /execute/stream
  │ 4. Proxy SSE frames → Browser
  ▼
ryeosd (:7400)
  │ 5. Verify signature, resolve directive:apps/tv-tracker/ai_chat
  │ 6. Inject db_context as input parameter
  │ 7. Spawn directive runtime (LLM loop)
  │ 8. Runtime reads db_context, calls backend-client tool for deep-dives
  │ 9. SSE events flow back: cognition_out → tool_call_* → thread_completed
  ▼
Backend
  │ 10. On cognition_out: forward token deltas to browser
  │ 11. On thread_completed: parse final outputs, update conversation memory
  ▼
Browser
  │ 12. Render streamed tokens in chat panel
  │ 13. Render tool cards from final outputs
```

**No webhooks needed.** The backend holds the SSE connection open for the
duration of the LLM call. The daemon's existing `/execute/stream` endpoint
does exactly this.

---

## SSE Event Types (daemon → backend → browser)

The daemon emits these events during directive execution. The backend
proxies them to the browser as-is.

| Event | Payload | Meaning |
|-------|---------|---------|
| `stream_started` | `{thread_id}` | Thread created |
| `stream_opened` | `{turn}` | Provider connection established |
| `cognition_out` | `{turn, delta}` | Token delta — stream to browser |
| `cognition_out` | `{turn, tool_use: {id, name, arguments}}` | LLM requesting a tool |
| `tool_call_start` | `{tool, call_id}` | Tool dispatch begins |
| `tool_call_result` | `{tool, call_id, ...}` | Tool finished |
| `thread_completed` | `{outcome_code, artifact_count}` | Done — directive outputs available |
| `thread_failed` | `{outcome_code, has_error}` | Directive errored |

### Getting the directive outputs

The `thread_completed` event signals completion but does **not** include the
directive return values directly. After seeing `thread_completed`, the backend
calls `GET /threads/{thread_id}` on the daemon to fetch the thread detail
including `result_json` (which contains the directive's `directive_return`
outputs: `response` and `tools`).

```
Backend receives thread_completed SSE event
  → thread_id from the stream_started event
  → GET http://127.0.0.1:7400/threads/{thread_id}
    (signed with x-rye-* headers, same as /execute/stream)
  → daemon returns:
    {
      "thread_id": "...",
      "status": "completed",
      "item_ref": "directive:apps/tv-tracker/ai_chat",
      "result": { "response": "...", "tools": [...] },
      ...
    }
  → backend parses result.response and result.tools
  → update conversation memory, frontend renders tool cards
```

Every caller is treated the same — the backend uses the same auth and the same
HTTP interface as any other client. No special parsing of SSE events needed.

**Status:** The `GET /threads/{id}` endpoint needs a new `thread_detail`
response mode added to the daemon. See "Daemon changes needed" below.

---

## Prerequisites

1. **ryeosd running** at `127.0.0.1:7400` with `require_auth: false` (dev mode)
   — or with an authorized key for the backend
2. **ClickHouse** with the TV ratings data
3. **Express backend** at `:4000`
4. **Node signing key** — the backend needs a key to sign `/execute/stream`
   requests. During dev, use the node's own key at
   `<state_dir>/.ai/node/identity/private_key.pem`

---

## Step 1: Create Rye Items

All items live in a project at `~/.ai/projects/network-tv-tracker/`.

### 1a. Knowledge: AI Identity

**File:** `~/.ai/projects/network-tv-tracker/.ai/knowledge/apps/tv-tracker/Identity.md`

```yaml
---
kind: knowledge
name: apps/tv-tracker/Identity
version: "1.0.0"
---

You are an AI analyst for Hong Kong TV ratings data. You have access to
a comprehensive database of television viewership metrics covering 8 free-to-air
channels and 600+ programs. You provide data-driven insights using specific
numbers, percentages, and comparisons.

Your expertise:
- TV Ratings Points (TVRs) and audience share analysis
- Channel performance comparison
- Program genre trends
- Time-slot analysis

You always cite specific numbers from the database context provided to you.
You respond in markdown with tables and structured analysis.
```

Sign it:
```bash
rye sign knowledge:apps/tv-tracker/Identity
```

### 1b. Knowledge: Analysis Behavior

**File:** `~/.ai/projects/network-tv-tracker/.ai/knowledge/apps/tv-tracker/AnalysisBehavior.md`

```yaml
---
kind: knowledge
name: apps/tv-tracker/AnalysisBehavior
version: "1.0.0"
---

# API Reference

The backend-client tool can call these endpoints on localhost:4000:

## GET /api/ratings/stats
Overall database statistics (total programs, ratings, channels, date range).

## GET /api/ratings/overview
Top 100 programs by TVRs for a given period.
Query params: `period` (7d, 30d, 90d), `demo` (all4+, etc.), `metric` (tvrs, share, reach)

## GET /api/ratings/channels
All 8 channels with aggregate metrics.

## GET /api/ratings/genres
All genres with program counts.

## GET /api/ratings/programs
All programs (compact form). Query params: `search`, `channel`, `genre`

## GET /api/ratings/program/:id
Single program detail with daily TVR timeline.

## GET /api/ratings/top10-history
Historical top-10 TVR rankings over time (for trend charts).

# Behavior Rules

1. **Answer from context first.** You receive `db_context` with full DB state
   pre-fetched. Use it for ~80% of questions without calling any tools.

2. **Use backend-client only for deep-dives.** Call the tool when the user
   needs data NOT in context: daily timelines, historical trends, specific
   program drill-downs.

3. **Always cite numbers.** "TVB Jade averaged 8.2 TVRs" not "TVB Jade did well".

4. **Return tool cards.** When your analysis would benefit from visualization,
   include a `tools` array in your directive_return with card configurations.

5. **Be concise.** The db_context is large (~50K chars). Don't repeat all of it
   — summarize and highlight.
```

Sign it:
```bash
rye sign knowledge:apps/tv-tracker/AnalysisBehavior
```

### 1c. Directive: ai_chat

**File:** `~/.ai/projects/network-tv-tracker/.ai/directives/apps/tv-tracker/ai_chat.md`

```yaml
---
kind: directive
name: apps/tv-tracker/ai_chat
version: "2.0.0"
inputs:
  - name: message
    type: string
    required: true
    description: "User's chat message"
  - name: history
    type: string
    required: false
    description: "JSON array of {role, content} conversation history"
  - name: db_context
    type: string
    required: false
    description: "Pre-fetched full DB state from backend"
  - name: active_tools
    type: string
    required: false
    description: "JSON array of currently active workspace tool configs"
outputs:
  - name: response
    type: string
    required: true
    description: "Markdown analysis with specific numbers"
  - name: tools
    type: string
    required: false
    description: "JSON array of tool card configs for frontend workspace"
context:
  system:
    - knowledge:apps/tv-tracker/Identity
    - knowledge:apps/tv-tracker/AnalysisBehavior
tools:
  - apps/tv-tracker/api/*
limits:
  turns: 8
  tokens: 65536
  spend: 0.10
  depth: 1
---

You are a TV ratings analyst. Answer the user's question using the database
context provided in `db_context`.

## Instructions

1. Read the `db_context` parameter — it contains the current database state.
2. Answer the user's question with specific numbers from the data.
3. If you need data NOT in db_context (daily timelines, historical trends),
   use the `backend-client` tool to call the backend API.
4. When done, call `directive_return` with:
   - `response`: your markdown analysis
   - `tools`: array of tool card configs (if applicable)

## Available tools

- `apps/tv-tracker/api/backend-client` — HTTP client for localhost:4000

## Response format

Always return:
```json
{
  "response": "## Analysis\n\n...\n\n| Program | TVRs | Share |\n|---|---|---|\n...",
  "tools": [{"type": "top10", "period": "30d"}, ...]
}
```
```

Sign it:
```bash
rye sign directive:apps/tv-tracker/ai_chat
```

### 1d. Tool: backend-client

**File:** `~/.ai/projects/network-tv-tracker/.ai/tools/apps/tv-tracker/api/backend-client.py`

```python
#!/usr/bin/env python3
"""HTTP client tool for the TV tracker backend API."""
import json
import sys
import urllib.request
import urllib.error

BACKEND_URL = "http://127.0.0.1:4000"

def main():
    params = json.loads(sys.stdin.read())
    method = params.get("method", "GET")
    path = params.get("path", "/api/ratings/stats")
    query = params.get("query", {})
    
    # Build URL
    url = BACKEND_URL + path
    if query:
        qs = "&".join(f"{k}={v}" for k, v in query.items())
        url += "?" + qs
    
    try:
        req = urllib.request.Request(url, method=method)
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read())
            print(json.dumps({"result": data}))
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        print(json.dumps({"error": f"HTTP {e.code}: {body}"}))
    except Exception as e:
        print(json.dumps({"error": str(e)}))

if __name__ == "__main__":
    main()
```

Sign it:
```bash
rye sign tool:apps/tv-tracker/api/backend-client
```

---

## Step 2: Backend Setup

### 2a. Request signing helper

The backend must sign requests to the daemon's `/execute/stream` endpoint
using the same protocol as the CLI (`ryeos-request-v1`).

```typescript
// src/rye-signer.ts
import crypto from 'crypto';
import fs from 'fs';
import { Ed25519PrivateKey } from '@lillux/crypto'; // or use noble-ed25519

// Load the signing key (node's own key during dev)
const KEY_PATH = process.env.RYE_CLI_KEY_PATH 
  || `${process.env.HOME}/.local/state/ryeosd/.ai/node/identity/private_key.pem`;

export interface SignHeaders {
  'x-rye-key-id': string;
  'x-rye-timestamp': string;
  'x-rye-nonce': string;
  'x-rye-signature': string;
}

export async function signRequest(
  method: string,
  pathAndQuery: string,
  body: Buffer
): Promise<SignHeaders> {
  // 1. Load Ed25519 private key from PEM
  const privateKey = loadPrivateKey(KEY_PATH);
  const publicKey = privateKey.getPublicKey();
  const fingerprint = sha256Hex(publicKey);
  
  // 2. Build canonical string
  const timestamp = Math.floor(Date.now() / 1000).toString();
  const nonce = crypto.randomBytes(16).toString('hex');
  const bodyHash = sha256Hex(body);
  const canonPath = canonicalizePath(pathAndQuery);
  const audience = fingerprint; // dev mode: self-referencing
  
  const stringToSign = [
    'ryeos-request-v1',
    method.toUpperCase(),
    canonPath,
    bodyHash,
    timestamp,
    nonce,
    audience,
  ].join('\n');
  
  // 3. Sign
  const contentHash = sha256Hex(Buffer.from(stringToSign));
  const signature = privateKey.sign(Buffer.from(contentHash, 'hex'));
  const sigB64 = signature.toString('base64');
  
  return {
    'x-rye-key-id': `fp:${fingerprint}`,
    'x-rye-timestamp': timestamp,
    'x-rye-nonce': nonce,
    'x-rye-signature': sigB64,
  };
}

function canonicalizePath(pq: string): string {
  const [path, query] = pq.split('?');
  if (!query) return path;
  const sorted = query.split('&').sort().join('&');
  return `${path}?${sorted}`;
}

function sha256Hex(data: Buffer | string): string {
  return crypto.createHash('sha256').update(data).digest('hex');
}
```

### 2b. SSE proxy endpoint

```typescript
// src/routes/ai-chat.ts
import { Router, Request, Response } from 'express';
import { signRequest } from '../rye-signer';

const router = Router();
const RYEOSD_URL = process.env.RYEOSD_URL || 'http://127.0.0.1:7400';

router.post('/chat', async (req: Request, res: Response) => {
  const { message, history, active_tools } = req.body;
  
  // 1. Pre-fetch DB state
  const dbContext = await buildDbContext();
  
  // 2. Build the execute/stream request body
  const body = JSON.stringify({
    item_ref: 'directive:apps/tv-tracker/ai_chat',
    project_path: process.env.RYE_PROJECT_PATH || 
      `${process.env.HOME}/.ai/projects/network-tv-tracker`,
    parameters: {
      message,
      history: history ? JSON.stringify(history) : undefined,
      db_context: dbContext,
      active_tools: active_tools ? JSON.stringify(active_tools) : undefined,
    },
  });
  const bodyBuf = Buffer.from(body);
  
  // 3. Sign the request
  const headers = await signRequest('POST', '/execute/stream', bodyBuf);
  
  // 4. Set up SSE response to browser
  res.setHeader('Content-Type', 'text/event-stream');
  res.setHeader('Cache-Control', 'no-cache');
  res.setHeader('Connection', 'keep-alive');
  res.flushHeaders();
  
  // 5. POST to daemon's /execute/stream, proxy SSE events
  const daemonRes = await fetch(`${RYEOSD_URL}/execute/stream`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...headers,
    },
    body,
  });
  
  if (!daemonRes.ok) {
    res.write(`event: stream_error\ndata: ${JSON.stringify({
      code: 'daemon_error',
      error: await daemonRes.text(),
    })}\n\n`);
    res.end();
    return;
  }
  
  // 6. Stream daemon SSE events to browser
  let threadId: string | null = null;
  const reader = daemonRes.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    
    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split('\n');
    buffer = lines.pop() || '';
    
    for (const line of lines) {
      // Forward all SSE frames to browser as-is
      if (line.startsWith('event:') || line.startsWith('data:') || line.startsWith('id:') || line === '') {
        res.write(line + '\n');
      }
      // Capture thread_id from stream_started
      if (!threadId && line.startsWith('data:')) {
        try {
          const payload = JSON.parse(line.slice(5).trim());
          if (payload.thread_id) threadId = payload.thread_id;
        } catch {}
      }
    }
  }
  
  // 7. Flush remaining
  if (buffer) res.write(buffer + '\n');
  res.write('\n');
  res.end();
  
  // 8. Fetch thread result from daemon via GET /threads/{id}
  if (threadId) {
    const resultPath = `/threads/${threadId}`;
    const resultHeaders = await signRequest('GET', resultPath, Buffer.alloc(0));
    const resultRes = await fetch(`${RYEOSD_URL}${resultPath}`, {
      headers: { ...resultHeaders },
    });
    if (resultRes.ok) {
      const thread = await resultRes.json();
      if (thread.result) {
        updateConversationMemory(req.session?.conversationId, thread.result);
      }
    }
  }
});
```

> **How outputs are retrieved:** The backend doesn't parse SSE events for
> return values. After the stream completes (`thread_completed`), it calls
> `GET /threads/{id}` — the same HTTP interface any caller would use. This
> endpoint needs a new `thread_detail` response mode (see Daemon changes below).

### 2c. DB context builder

```typescript
// src/db-context.ts

export async function buildDbContext(): Promise<string> {
  const [stats, overview, channels, genres, programs] = await Promise.all([
    fetchJson('/api/ratings/stats'),
    fetchJson('/api/ratings/overview?period=30d'),
    fetchJson('/api/ratings/channels'),
    fetchJson('/api/ratings/genres'),
    fetchJson('/api/ratings/programs'),
  ]);
  
  const lines: string[] = [
    '=== HK TV RATINGS DATABASE STATE ===',
    `Data range: ${stats.dateRange.start} to ${stats.dateRange.end}`,
    `Programs: ${stats.totalPrograms}, Rating records: ${stats.totalRatings}, Channels: ${stats.totalChannels}`,
  ];
  
  // Channel performance
  lines.push('', '--- CHANNEL PERFORMANCE (30d) ---');
  for (const ch of channels) {
    lines.push(`${ch.name}: broadcasts=${ch.broadcasts}, avg_tvrs=${ch.avgTvrs}, avg_reach%=${ch.avgReach}, avg_mins=${ch.avgMins}, total_tvrs=${ch.totalTvrs}`);
  }
  
  // Top 100 programs
  lines.push('', '--- TOP 100 PROGRAMS BY TVRs (30d, All 4+) ---');
  overview.slice(0, 100).forEach((p: any, i: number) => {
    lines.push(`#${i+1} ${p.name} [${p.genre}] on ${p.channel}: tvrs=${p.tvrs}, share%=${p.share}, reach%=${p.reach}`);
  });
  
  // All programs (compact)
  lines.push('', '--- ALL PROGRAMS (compact) ---');
  for (const p of programs) {
    lines.push(`id=${p.id} ${p.name} [${p.genre}] on ${p.channel}: tvrs=${p.tvrs}, share%=${p.share}, reach%=${p.reach}`);
  }
  
  return lines.join('\n');
}

async function fetchJson(path: string): Promise<any> {
  const base = process.env.BACKEND_URL || 'http://127.0.0.1:4000';
  const res = await fetch(base + path);
  return res.json();
}
```

---

## Step 3: Frontend

### 3a. Streaming chat hook

```typescript
// src/hooks/useStreamingChat.ts

export function useStreamingChat() {
  const [response, setResponse] = useState('');
  const [tools, setTools] = useState([]);
  const [isStreaming, setIsStreaming] = useState(false);
  
  const sendMessage = useCallback(async (message: string) => {
    setIsStreaming(true);
    setResponse('');
    setTools([]);
    
    const res = await fetch('/api/ai/chat', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ message }),
    });
    
    const reader = res.body!.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      
      buffer += decoder.decode(value, { stream: true });
      
      // Parse SSE events
      const events = buffer.split('\n\n');
      buffer = events.pop() || '';
      
      for (const event of events) {
        const eventType = event.match(/^event:\s*(.+)/m)?.[1];
        const data = event.match(/^data:\s*(.+)/m)?.[1];
        if (!data) continue;
        
        try {
          const payload = JSON.parse(data);
          
          switch (eventType) {
            case 'stream_started':
              // thread_id available in payload if needed
              break;
            case 'cognition_out':
              // Token delta — append to response
              if (payload.delta) {
                setResponse(prev => prev + payload.delta);
              }
              break;
              
            case 'tool_call_start':
              // Show "calling tool..." indicator
              break;
              
            case 'tool_call_result':
              // Tool completed
              break;
              
            case 'thread_completed':
              // Stream done. Backend has already fetched the
              // full result via GET /threads/{id} and updated
              // conversation memory. Browser gets tool cards
              // from the backend's /api/ai/conversation endpoint.
              setIsStreaming(false);
              break;
              
            case 'stream_error':
              setIsStreaming(false);
              console.error('Stream error:', payload);
              break;
          }
        } catch {}
      }
    }
    
    setIsStreaming(false);
  }, []);
  
  return { response, tools, isStreaming, sendMessage };
}
```

---

## Step 4: Register the directive with the daemon

The daemon resolves items from project/user/system spaces. The project space
needs to be at the path the backend sends as `project_path`:

```bash
# Create project directory
mkdir -p ~/.ai/projects/network-tv-tracker/.ai/{knowledge/apps/tv-tracker,directives/apps/tv-tracker,tools/apps/tv-tracker/api}

# Copy items into place (after signing)
# ... or create them directly in the project directory

# Verify resolution:
rye execute directive:apps/tv-tracker/ai_chat --project-path ~/.ai/projects/network-tv-tracker
```

---

## Step 5: Test the full flow

### 5a. Test daemon directly

```bash
# Health check
curl http://127.0.0.1:7400/health

# Execute the directive (non-streaming, for testing)
curl -X POST http://127.0.0.1:7400/execute \
  -H "Content-Type: application/json" \
  -d '{
    "item_ref": "directive:apps/tv-tracker/ai_chat",
    "project_path": "'$HOME'/.ai/projects/network-tv-tracker",
    "parameters": {
      "message": "What are the top 5 programs?",
      "db_context": "=== HK TV RATINGS ===\nTest data..."
    }
  }'
```

### 5b. Test streaming

```bash
curl -N -X POST http://127.0.0.1:7400/execute/stream \
  -H "Content-Type: application/json" \
  -H "x-rye-key-id: fp:$(cat ~/.local/state/ryeosd/.ai/node/identity/public-identity.json | jq -r .fingerprint | sed 's/fp://')" \
  -d '{
    "item_ref": "directive:apps/tv-tracker/ai_chat",
    "project_path": "'$HOME'/.ai/projects/network-tv-tracker",
    "parameters": {
      "message": "What are the top 5 programs?"
    }
  }'
```

> Note: The above curl won't work without proper signing headers.
> Use the backend's signed proxy instead.

### 5c. Test end-to-end

```bash
# Start daemon
ryeosd &

# Start backend
PORT=4000 node dist/index.js &

# Test via backend
curl -N -X POST http://localhost:4000/api/ai/chat \
  -H "Content-Type: application/json" \
  -d '{"message": "What are the top 5 dramas by TVRs?"}'
```

---

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `RYEOSD_URL` | `http://127.0.0.1:7400` | Daemon address |
| `RYE_CLI_KEY_PATH` | `<state_dir>/.ai/node/identity/private_key.pem` | Signing key for daemon auth |
| `RYE_PROJECT_PATH` | `$HOME/.ai/projects/network-tv-tracker` | Project root with Rye items |
| `BACKEND_PORT` | `4000` | Express backend port |
| `BACKEND_URL` | `http://127.0.0.1:4000` | Backend self-reference for DB fetch |
| `CLICKHOUSE_URL` | (required) | ClickHouse connection string |

---

## Daemon Changes Needed

### Change 1: `read` response mode + `read_source` plugin system (required)

The daemon has three response modes today:

| Mode | What it does | Plugin system |
|------|-------------|---------------|
| `static` | Fixed bytes baked at boot | None |
| `launch` | Dispatch work, return 202 | None |
| `event_stream` | SSE stream | `StreamingSource` trait (2 builtins: `dispatch_launch`, `thread_events`) |

What's missing: a mode for **synchronous state reads** — "given path captures, produce JSON from the state store." Thread detail is the first instance, but the same pattern covers `GET /threads`, `GET /threads/{id}/events`, `GET /threads/{id}/artifacts`, etc.

The answer isn't a `thread_detail` mode. It's a **`read` mode** backed by a **`ReadSource` plugin system** — the exact same architecture as `event_stream` + `StreamingSource`, but for unary JSON responses instead of SSE.

**What to build:**

**A. New response mode** `read` (`ryeosd/src/routes/response_modes/read_mode.rs`)

Exact same structure as `event_stream_mode.rs`:
- Compile: validates `response.source` is a known `ReadSource` key, passes `source_config` to it
- Handle: delegates to the bound source's `fetch()`, returns `200 JSON` or `404`/`500`
- Register in `ResponseModeRegistry::with_builtins()`

**B. Read source plugin system** (`ryeosd/src/routes/read_sources/mod.rs`)

Mirrors `streaming_sources/` exactly:

```rust
/// Compile-time: validates route YAML config, returns a bound source.
pub trait ReadSource: Send + Sync {
    fn key(&self) -> &'static str;
    fn compile(
        &self,
        raw_route: &RawRouteSpec,
        source_config: &Value,
        ctx: &SourceCompileContext,
    ) -> Result<Arc<dyn BoundReadSource>, RouteConfigError>;
}

/// Runtime: produces JSON from path captures + app state.
#[axum::async_trait]
pub trait BoundReadSource: Send + Sync {
    async fn fetch(
        &self,
        captures: &HashMap<String, String>,
        state: &AppState,
    ) -> Result<Option<Value>, RouteDispatchError>;
}

pub struct ReadSourceRegistry { /* same pattern as StreamingSourceRegistry */ }
```

**C. First builtin: `thread_detail`** (`ryeosd/src/routes/read_sources/thread_detail.rs`)

- Compile: validates `source_config` has `thread_id: "${path.thread_id}"`, extracts the capture name
- Fetch: calls `state.state_store.get_thread()` + `get_thread_result()` + `list_thread_artifacts()` + `get_facets()`, returns the merged JSON (same shape as the existing `service:threads/get` handler)
- Returns `None` → mode produces `404`

**D. Route YAML** (`ryeos-bundles/core/.ai/node/routes/threads-detail.yaml`):

```yaml
category: "routes"
section: routes
id: threads/detail
path: /threads/{thread_id}
methods:
  - GET
auth: rye_signed
limits:
  body_bytes_max: 0
  timeout_ms: 5000
request:
  body: none
response:
  mode: read
  source: thread_detail
  source_config:
    thread_id: "${path.thread_id}"
```

**Future endpoints are just YAML + source struct:**

| Endpoint | Route YAML | ReadSource |
|----------|-----------|------------|
| `GET /threads/{id}` | `threads-detail.yaml` | `thread_detail` |
| `GET /threads` | `threads-list.yaml` | `thread_list` |
| `GET /threads/{id}/events` | `threads-events.yaml` | `thread_events_page` |
| `GET /status` | `status.yaml` | `system_status` |

No new response modes. No new service handlers. Same plugin pattern, same data-driven routes.

**Note:** The existing `service:threads/get` handler already does exactly what `thread_detail` needs (calls `get_thread`, `get_thread_result`, `list_thread_artifacts`, `get_facets`). The read source can reuse that logic — the difference is it's invoked via the route table, not the service executor.

### Change 2: Auth for external callers (required for production)

Currently `require_auth: false` skips auth entirely. For production:

1. Create an authorized key TOML for the backend's signing key:
   ```toml
   # <state_dir>/.ai/node/auth/authorized_keys/<backend-fp>.toml
   fingerprint = "<backend-key-fingerprint>"
   public_key = "ed25519:<base64>"
   scopes = ["execute", "threads:read"]
   label = "tv-tracker-backend"
   ```
2. Set `require_auth: true` in `config.yaml`

For dev, `require_auth: false` works fine.

### Change 3: Provider config (required)

The directive needs an LLM provider. This is configured as a provider item
in the project or via node config. The daemon passes API keys from the vault
or environment to the runtime subprocess.

```yaml
# In the node config or as a provider item
providers:
  - name: deepseek
    model: deepseek-chat
    auth:
      env_var: DEEPSEEK_API_KEY
    base_url: https://api.deepseek.com
```

### Change 4: Tool handler for `.py` files (required if using Python tool)

The `backend-client` tool is a Python script. The daemon needs a Python
interpreter handler registered in the kind schema. Alternatively, rewrite
as a shell script using `curl`, or as a native binary.

---

## Tool Card Types (frontend)

| Type | Chart | Controls | Data Source |
|------|-------|----------|-------------|
| `top10` | Horizontal bar | Period, Metric, Demo | `/api/ratings/overview` |
| `channel_compare` | Grouped bar | Period | `/api/ratings/channels` |
| `program_detail` | Line chart | Time range (7/14/30D) | `/api/ratings/program/:id` |
| `genre_breakdown` | Donut | Refresh | `/api/ratings/genres` |
| `trend_line` | Multi-line | Period | `/api/ratings/top10-history` |
| `rankings_table` | Scrollable table | Period, Demo, Metric | `/api/ratings/programs` |

Frontend fetches card data directly from the backend API — the LLM only
decides *which* cards to show, not the data itself.
