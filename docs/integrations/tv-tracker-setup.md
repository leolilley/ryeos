# TV Tracker — Rye OS Integration Setup Guide

End-to-end guide for integrating a TV ratings tracker app with Rye OS.
The backend (Express) proxies SSE streams from the daemon to the browser.
**No daemon changes required** — the existing `/execute/stream` and
`/threads/{thread_id}` endpoints satisfy the full chat flow.

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
  │ 11. On thread_completed: GET /threads/{thread_id} → parse final outputs
  │ 12. Update conversation memory with response + tool cards
  ▼
Browser
  │ 13. Render streamed tokens in chat panel
  │ 14. Render tool cards from final outputs
```

**No webhooks.** The backend holds the SSE connection open for the
duration of the LLM call. The daemon's `/execute/stream` endpoint does
exactly this.

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

The `thread_completed` event signals completion but does NOT include the
directive return values directly. After seeing `thread_completed`, the
backend calls `GET /threads/{thread_id}` on the daemon to fetch the
thread detail including `result` (which contains the directive's
`directive_return` outputs: `response` and `tools`).

This endpoint is **already wired** in the daemon: `threads-detail.yaml`
maps `GET /threads/{thread_id}` → `json` response mode →
`service:threads/get` handler. No additional daemon code needed.

```
Backend receives thread_completed SSE event
  → thread_id from the stream_started event
  → GET http://127.0.0.1:7400/threads/{thread_id}
    (signed with x-ryeos-* headers, same as /execute/stream)
  → daemon returns:
    {
      "thread_id": "...",
      "status": "completed",
      "item_ref": "directive:apps/tv-tracker/ai_chat",
      "result": { "response": "...", "tools": [...] },
      ...
    }
  → backend parses result.response and result.tools
```

---

## Prerequisites

1. **ryeosd** — local (`ryeosd` binary) or containerized
   (`ghcr.io/leolilley/ryeosd-full` image). Production setups require
   authenticated requests (see Production Deployment).
2. **ClickHouse** with the TV ratings data.
3. **Express backend** at `:4000`.
4. **Ed25519 signing key** — the backend needs a keypair to sign
   `/execute/stream` requests. During dev, use the node's own key at
   `<state_dir>/.ai/node/identity/private_key.pem`. In production,
   generate a dedicated client key and authorize it (Production Step 4).

---

## Step 1: Create Rye Items

All items live in a project at `~/.ai/projects/network-tv-tracker/`.

> **Implementation note:** during build-out, ignore the per-item
> `ryeos sign` snippets shown below. The canonical project-tree
> deployment step is `ryeos publish . --key <pem> --owner <label>` once
> the project tree is ready (see Production Step 1). Use per-item
> signing only when iterating on a single file mid-development. For
> bundle items (`ryeos-bundles/{core,standard}`), use
> `./scripts/populate-bundles.sh` instead — see
> [docs/operations/signing-bundles.md](../operations/signing-bundles.md).

### 1a. Knowledge: AI Identity

**File:** `~/.ai/projects/network-tv-tracker/.ai/knowledge/apps/tv-tracker/Identity.md`

```yaml
---
kind: knowledge
name: apps/tv-tracker/Identity
version: "1.0.0"
---

You are an AI analyst for Hong Kong TV ratings data. You have access to
a comprehensive database of television viewership metrics covering 8
free-to-air channels and 600+ programs. You provide data-driven insights
using specific numbers, percentages, and comparisons.

Your expertise:
- TV Ratings Points (TVRs) and audience share analysis
- Channel performance comparison
- Program genre trends
- Time-slot analysis

You always cite specific numbers from the database context provided to
you. You respond in markdown with tables and structured analysis.
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

1. **Answer from context first.** You receive `db_context` with full DB
   state pre-fetched. Use it for ~80% of questions without calling any tools.

2. **Use backend-client only for deep-dives.** Call the tool when the
   user needs data NOT in context: daily timelines, historical trends,
   specific program drill-downs.

3. **Always cite numbers.** "TVB Jade averaged 8.2 TVRs" not "TVB Jade
   did well".

4. **Return tool cards.** When your analysis would benefit from
   visualization, include a `tools` array in your directive_return with
   card configurations.

5. **Be concise.** The db_context is large (~50K chars). Don't repeat all
   of it — summarize and highlight.
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

You are a TV ratings analyst. Answer the user's question using the
database context provided in `db_context`.

## Instructions

1. Read the `db_context` parameter — it contains the current database state.
2. Answer the user's question with specific numbers from the data.
3. If you need data NOT in db_context (daily timelines, historical
   trends), use the `backend-client` tool to call the backend API.
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

> **Python tool runtime:** the daemon needs a Python interpreter handler
> registered to execute `.py` tools. The standard bundle ships with one
> at `ryeos/core/runtimes/python/script.yaml`. If you prefer to avoid
> Python in the project tree, rewrite this as a shell script using
> `curl` or as a native binary.

---

## Step 2: Backend Setup

### 2a. Request signing helper

The backend signs requests to the daemon's `/execute/stream` and
`/threads/{thread_id}` endpoints using the `ryeos-request-v1` protocol.

```typescript
// src/ryeos-request-signer.ts
import crypto from 'crypto';
import fs from 'fs';
import { Ed25519PrivateKey } from '@lillux/crypto'; // or use noble-ed25519

// Load the signing key (node's own key during dev)
const KEY_PATH = process.env.RYEOS_CLI_KEY_PATH
  || `${process.env.HOME}/.local/state/ryeosd/.ai/node/identity/private_key.pem`;

export interface SignHeaders {
  'x-ryeos-key-id': string;
  'x-ryeos-timestamp': string;
  'x-ryeos-nonce': string;
  'x-ryeos-signature': string;
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
    'x-ryeos-key-id': `fp:${fingerprint}`,
    'x-ryeos-timestamp': timestamp,
    'x-ryeos-nonce': nonce,
    'x-ryeos-signature': sigB64,
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
import { signRequest } from '../ryeos-request-signer';

const router = Router();
const RYEOSD_URL = process.env.RYEOSD_URL || 'http://127.0.0.1:7400';

router.post('/chat', async (req: Request, res: Response) => {
  const { message, history, active_tools } = req.body;

  // 1. Pre-fetch DB state
  const dbContext = await buildDbContext();

  // 2. Build the execute/stream request body
  const body = JSON.stringify({
    item_ref: 'directive:apps/tv-tracker/ai_chat',
    project_path: process.env.RYEOS_PROJECT_PATH ||
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

  // 8. Fetch thread result from daemon via GET /threads/{thread_id}
  //    The daemon route threads-detail.yaml maps this to
  //    json response mode → service:threads/get handler.
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
              // full result via GET /threads/{thread_id} and updated
              // conversation memory. Browser fetches tool cards
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

The daemon resolves items from project/user/system spaces. The project
space needs to be at the path the backend sends as `project_path`.

### Local dev

```bash
# Create project directory
mkdir -p ~/.ai/projects/network-tv-tracker/.ai/{knowledge/apps/tv-tracker,directives/apps/tv-tracker,tools/apps/tv-tracker/api}

# Create the items per Step 1 (or copy from a checkout)

# Publish + sign in one step:
ryeos publish ~/.ai/projects/network-tv-tracker \
  --key ~/.dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev

# Pin the publisher trust on the daemon (one-time):
ryeos trust pin --from ~/.ai/projects/network-tv-tracker/PUBLISHER_TRUST.toml

# Verify resolution:
ryeos execute directive:apps/tv-tracker/ai_chat \
  --project-path ~/.ai/projects/network-tv-tracker
```

### Containerized production

In production, the tv-tracker project tree is mounted into the daemon
container at a stable path. See **Production Deployment** below.

The `project_path` in requests must match the **in-container mount
path**, not the host path.

---

## Step 5: Test the full flow

### 5a. Test the daemon's threads-detail route

Before wiring the backend, sanity-check that `GET /threads/{id}` works
end-to-end. Run a directive once via `/execute`, capture the
`thread_id`, then read the result back via `/threads/{thread_id}`:

```bash
# Health check
curl http://127.0.0.1:7400/health

# Execute the directive (non-streaming)
THREAD=$(curl -sS -X POST http://127.0.0.1:7400/execute \
  -H "Content-Type: application/json" \
  -d '{
    "item_ref": "directive:apps/tv-tracker/ai_chat",
    "project_path": "'$HOME'/.ai/projects/network-tv-tracker",
    "parameters": {
      "message": "What are the top 5 programs?",
      "db_context": "=== HK TV RATINGS ===\nTest data..."
    }
  }' | jq -r .thread_id)
echo "thread_id=$THREAD"

# Fetch the result via /threads/{thread_id}
# (works because threads-detail.yaml maps this route to json mode +
#  service:threads/get — no extra daemon code required)
curl -sS http://127.0.0.1:7400/threads/$THREAD | jq .result
```

If both calls return successfully, the daemon-side chat plumbing is
proven. Move on to the streaming path.

### 5b. Test streaming

```bash
curl -N -X POST http://127.0.0.1:7400/execute/stream \
  -H "Content-Type: application/json" \
  -H "x-ryeos-key-id: fp:$(jq -r .fingerprint ~/.local/state/ryeosd/.ai/node/identity/public-identity.json | sed 's/fp://')" \
  -d '{
    "item_ref": "directive:apps/tv-tracker/ai_chat",
    "project_path": "'$HOME'/.ai/projects/network-tv-tracker",
    "parameters": { "message": "What are the top 5 programs?" }
  }'
```

> Note: the curl above is missing the `x-ryeos-signature` header, which
> requires Ed25519 signing. Use the backend's signed proxy for real
> requests.

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
| `RYEOS_CLI_KEY_PATH` | `<state_dir>/.ai/node/identity/private_key.pem` | Signing key for daemon auth |
| `RYEOS_PROJECT_PATH` | `$HOME/.ai/projects/network-tv-tracker` | Project root with Rye items |
| `BACKEND_PORT` | `4000` | Express backend port |
| `BACKEND_URL` | `http://127.0.0.1:4000` | Backend self-reference for DB fetch |
| `CLICKHOUSE_URL` | (required) | ClickHouse connection string |

---

## Production Deployment

This section covers running the daemon in a container with the
tv-tracker project mounted in, authenticated requests, and publisher
trust.

### Volume contract

The daemon container expects a single persistent volume mounted at `/data`:

```
/data
├── core/                  # System space (RYEOS_SYSTEM_SPACE_DIR)
│   └── .ai/
│       ├── node/           # Node identity, auth keys, vault
│       ├── engine/kinds/   # Core bundle (from image, updated on boot)
│       ├── bundles/        # Installed bundles
│       └── state/          # CAS objects, refs
├── user/                  # User space (HOME=/data/user)
│   └── .ai/
│       └── config/keys/    # Operator trust store, user signing key
└── projects/              # Consumer project mounts
    └── network-tv-tracker/
        └── .ai/           # tv-tracker directives, tools, knowledge
```

Both `/data/core` and `/data/user` persist across container redeploys.
The entrypoint runs `ryeos init` on every boot (idempotent) to keep
bundles current with the image.

### Step 1: Publish the tv-tracker project

The daemon only loads items signed by a key in the operator's trust
store. Before mounting the project, sign all its items:

```bash
# From the network-tv-tracker repo root (on your build machine)
ryeos publish . \
  --key path/to/tv-tracker-publisher.pem \
  --owner tv-tracker-team
```

This produces `./PUBLISHER_TRUST.toml` in the project root — keep this
file, you'll need it to pin trust on the daemon.

### Step 2: Pin the tv-tracker publisher key

The daemon must trust the key that signed the items:

```bash
# Run inside the daemon container (or against the same /data volume)
ryeos trust pin \
  --from /data/projects/network-tv-tracker/PUBLISHER_TRUST.toml \
  --user-root /data/user
```

This writes a trust doc to
`/data/user/.ai/config/keys/trusted/<fp>.toml`. After this step, the
daemon will load and verify signed items from the project.

### Step 3: Mount the project into the container

Mount the published project tree into the container at a stable path.
The `project_path` in API requests must match this in-container path.

**docker run:**

```bash
docker run -d \
  -v ryeos-data:/data \
  -v /host/path/to/network-tv-tracker:/data/projects/network-tv-tracker:ro \
  -p 7400:8000 \
  ghcr.io/leolilley/ryeosd-full:0.3.0
```

**Docker Compose:**

```yaml
services:
  ryeosd:
    image: ghcr.io/leolilley/ryeosd-full:0.3.0
    volumes:
      - ryeos-data:/data
      - ./network-tv-tracker:/data/projects/network-tv-tracker:ro
    ports:
      - "7400:8000"

volumes:
  ryeos-data:
```

**Railway / Fly.io:** use a persistent volume mounted at `/data`, and a
separate mount or build step to place the project at
`/data/projects/network-tv-tracker`.

### Step 4: Authorize the backend client

All authenticated endpoints (including `/execute/stream` and
`/threads/{id}`) require requests signed by an Ed25519 key whose
fingerprint appears in an authorized-key TOML under
`<system-space>/.ai/node/auth/authorized_keys/`. That TOML must itself
be signed by the node identity key.

Generate a keypair for the backend and authorize its public key:

```bash
# 1. Generate a signing keypair for the backend (on your build machine)
openssl genpkey -algorithm ED25519 -out tv-tracker-backend.pem

# 2. Extract the raw 32-byte public key as base64
#    (Ed25519 PKCS#8 DER: last 32 bytes are the raw key)
PUBKEY_B64=$(python3 -c "
import base64, subprocess
der = subprocess.run(['openssl', 'pkey', '-in', 'tv-tracker-backend.pem',
                       '-pubout', '-outform', 'DER'],
                      capture_output=True).stdout
print(base64.b64encode(der[-32:]).decode())
")

# 3. Authorize the key on the daemon
#    (run inside the container, or from a machine with access to /data)
ryeos-core-tools authorize-client \
  --system-space-dir /data/core \
  --public-key "$PUBKEY_B64" \
  --scopes '*' \
  --label "tv-tracker-backend"
```

This writes a node-signed TOML to
`/data/core/.ai/node/auth/authorized_keys/<fp>.toml`. The daemon loads
authorized keys at startup. After running this command, restart the
daemon (or rely on hot-reload if supported).

The backend uses the private key (`tv-tracker-backend.pem`) to sign each
request. See `signRequest` in Step 2a for the signing protocol.

### Step 5: Provision provider API keys in the vault

Provider keys (e.g. `ZEN_API_KEY`, `OPENROUTER_API_KEY`) live in the
daemon's encrypted vault — they never live in container env, image
build args, or `.env` files. The daemon scans the provider configs in
the system bundle, auto-discovers which `auth.env_var` keys are needed,
and injects them into the directive runtime's env at spawn time.
Directives do not need to declare `required_secrets` for provider keys
— the daemon handles this automatically.

```bash
# Put the provider key in the vault (run inside the container)
docker exec ryeosd ryeos-core-tools vault put \
  --system-space-dir /data/core \
  ZEN_API_KEY=sk-actual-key-value
```

> The `vault put` subcommand accepts `KEY=VALUE` pairs as positional
> arguments. To pipe a value from stdin, use `KEY=$(cat)` syntax in the
> shell.

Verify the key is stored:

```bash
docker exec ryeosd ryeos-core-tools vault list \
  --system-space-dir /data/core
```

The output lists key names (never values). The key persists across
container redeploys — no re-`vault put` needed.

If the operator hasn't provisioned a key the runtime needs, the runtime
fails with a typed error naming the missing env var and the remediation
command.

### Step 6: Configure the backend

Set these environment variables on the backend:

| Variable | Value | Notes |
|----------|-------|-------|
| `RYEOSD_URL` | `http://ryeosd:8000` | Daemon address (container network) |
| `RYEOS_CLI_KEY_PATH` | `/path/to/tv-tracker-backend.pem` | Client signing key |
| `RYEOS_PROJECT_PATH` | `/data/projects/network-tv-tracker` | In-container project path |

> Provider API keys (e.g. `ZEN_API_KEY`) do NOT go in the backend's
> env. They live in the daemon's encrypted vault (Step 5).

### Step 7: Verify end-to-end

```bash
# From a machine that can reach the daemon
# (replace <fp> with the backend key's fingerprint)
FP=$(python3 -c "
import base64, subprocess, hashlib
der = subprocess.run(['openssl', 'pkey', '-in', 'tv-tracker-backend.pem',
                       '-pubout', '-outform', 'DER'],
                      capture_output=True).stdout
print(hashlib.sha256(der[-32:]).hexdigest())
")

# (signature header omitted — use the backend's signed proxy for real calls)
curl -N -X POST http://localhost:7400/execute/stream \
  -H "Content-Type: application/json" \
  -H "x-ryeos-key-id: fp:${FP}" \
  -H "x-ryeos-timestamp: $(date +%s)" \
  -H "x-ryeos-nonce: $(openssl rand -hex 16)" \
  -d '{
    "item_ref": "directive:apps/tv-tracker/ai_chat",
    "project_path": "/data/projects/network-tv-tracker",
    "parameters": { "message": "What are the top 5 programs?" }
  }'
```

### Redeploy safety

The entrypoint runs `ryeos init` on every boot. Both operator trust
(`/data/user/.ai/config/keys/trusted/`) and node state
(`/data/core/.ai/node/`) persist on the volume. Re-pulling the image or
restarting the container preserves all state — no manual re-bootstrap
needed.

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
