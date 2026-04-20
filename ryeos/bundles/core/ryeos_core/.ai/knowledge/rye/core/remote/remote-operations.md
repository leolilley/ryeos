<!-- rye:signed:2026-04-19T09:49:53Z:69ca8fd6f759aa6a5d27ac317e16b934d4ae90300c7701155fdb398fd1e95ecb:ERJQncf8XIFnH1zGeIEpwQjyS+4npnVcT8FAw4PEcdZQam/tfsAbPFVHypSo2Rkt4XzmmwQ3BzX2xbMMmYMNCA==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb

```yaml
name: remote-operations
title: "Remote Tool — Operations Reference"
entry_type: reference
category: rye/core/remote
version: "1.0.0"
author: rye-os
created_at: 2026-03-17T00:00:00Z
tags:
  - remote
  - operations
  - debugging
  - cas
  - execution
```

# Remote Tool — Operations Reference

Agent-facing reference for `rye/core/remote/remote` — the client-side tool for interacting with ryeos-node.

## Tool Actions

The remote tool (`rye/core/remote/remote`) supports 9 actions:

| Action           | Purpose                                        | Key params                                     |
| ---------------- | ---------------------------------------------- | ---------------------------------------------- |
| `push`           | Sync project + user space objects to remote    | `remote`                                       |
| `pull`           | Fetch CAS objects by hash from remote          | `hashes[]`, `remote`                           |
| `status`         | Show local vs remote manifest hashes           | `remote`                                       |
| `execute`        | Push + trigger remote execution + pull results | `item_type`, `item_id`, `parameters`, `thread` |
| `threads`        | List recent remote executions                  | `limit`, `project_path`                        |
| `thread_status`  | Get status of a specific remote thread         | `thread_id`                                    |
| `secrets_push`   | Push secrets from .env file or env vars        | `env_file` or `names[]`                        |
| `secrets_list`   | List secret names stored on remote (no values) | —                                              |
| `secrets_remove` | Remove a named secret from remote vault        | `secret_name`                                  |

All actions accept an optional `remote` param to target a named remote (default: `"default"`).

## Server Endpoints

The remote server exposes these HTTP endpoints:

### CAS Object Sync

| Method | Endpoint       | Purpose                                | Auth scope    |
| ------ | -------------- | -------------------------------------- | ------------- |
| POST   | `/objects/has` | Check which hashes exist in user CAS   | `remote:push` |
| POST   | `/objects/put` | Upload objects to user CAS             | `remote:push` |
| POST   | `/objects/get` | Download objects from user CAS by hash | `remote:push` |

### Project & User Space

| Method | Endpoint           | Purpose                                 | Auth scope    |
| ------ | ------------------ | --------------------------------------- | ------------- |
| POST   | `/push`            | Push project manifest + create snapshot | `remote:push` |
| POST   | `/push/user-space` | Push user space manifest independently  | `remote:push` |
| GET    | `/user-space`      | Get current user space ref              | `remote:push` |

### Execution

| Method | Endpoint   | Purpose                                       | Auth scope       |
| ------ | ---------- | --------------------------------------------- | ---------------- |
| POST   | `/execute` | Execute a tool or directive from project HEAD | `remote:execute` |
| POST   | `/search`  | Search items on remote (wraps execute)        | `remote:execute` |
| POST   | `/load`    | Load/inspect item on remote (wraps execute)   | `remote:execute` |
| POST   | `/sign`    | Sign item on remote (wraps execute)           | `remote:execute` |

### Threads & History

| Method | Endpoint               | Purpose                               | Auth scope       |
| ------ | ---------------------- | ------------------------------------- | ---------------- |
| GET    | `/threads`             | List user's remote executions         | `remote:threads` |
| GET    | `/threads/{thread_id}` | Get specific thread status            | `remote:threads` |
| GET    | `/history`             | Walk snapshot chain from project HEAD | `remote:threads` |

### Secrets

| Method | Endpoint          | Purpose                                   | Auth scope       |
| ------ | ----------------- | ----------------------------------------- | ---------------- |
| POST   | `/secrets`        | Upsert user secrets into vault            | `remote:secrets` |
| GET    | `/secrets`        | List secret names (values never returned) | `remote:secrets` |
| DELETE | `/secrets/{name}` | Delete a secret by name                   | `remote:secrets` |

### Webhook Bindings

| Method | Endpoint                      | Purpose                                | Auth scope                |
| ------ | ----------------------------- | -------------------------------------- | ------------------------- |
| POST   | `/webhook-bindings`           | Create a webhook binding               | `remote:webhook-bindings` |
| GET    | `/webhook-bindings`           | List user's webhook bindings           | `remote:webhook-bindings` |
| DELETE | `/webhook-bindings/{hook_id}` | Revoke a webhook binding (soft delete) | `remote:webhook-bindings` |

### Infra

| Method | Endpoint      | Purpose                     | Auth |
| ------ | ------------- | --------------------------- | ---- |
| GET    | `/health`     | Health check                | None |
| GET    | `/public-key` | Server's Ed25519 public key | None |

## Using the Remote Tool (via MCP)

### Direct execution (tools)

Tools execute inline on remote — results return immediately:

```
mcp__rye__execute(
  item_type="tool",
  item_id="rye/email/send",
  project_path="/home/user/project",
  target="remote",
  parameters={"to": "user@example.com", "subject": "Hello", "body": "..."}
)
```

The MCP `target="remote"` parameter routes through the remote tool automatically. This:

1. Pushes any changed objects to remote CAS
2. POSTs to `/execute` with `thread="inline"`
3. Pulls result objects back
4. Returns the result

### Direct execution (directives)

Directives on remote always use `thread="fork"` — an LLM thread is spawned server-side:

```
mcp__rye__execute(
  item_type="directive",
  item_id="rye/email/draft_response",
  project_path="/home/user/project",
  target="remote",
  parameters={"email_body": "...", "email_subject": "..."}
)
```

The client-side remote tool enforces this: `item_type="directive"` with `thread != "fork"` is rejected before hitting the server.

### Using the remote tool directly

For operations that aren't execute (push, pull, threads, secrets), invoke the tool directly:

```
mcp__rye__execute(
  item_type="tool",
  item_id="rye/core/remote/remote",
  project_path="/home/user/project",
  parameters={"action": "threads", "limit": 5}
)
```

## Debugging Remote Executions

### Step 1: Check thread status

```
parameters={"action": "threads", "limit": 5}
```

Look at the `state` field:

- `running` — still executing
- `completed` — finished successfully
- `error` — failed (check CAS objects for details)

Get details for a specific thread:

```
parameters={"action": "thread_status", "thread_id": "rye-remote-abc123"}
```

Key fields in thread status:

- `state` — running/completed/error
- `snapshot_hash` — project snapshot created by this execution
- `runtime_outputs_bundle_hash` — hash of all output files (transcripts, knowledge, etc.)
- `project_manifest_hash` — which version of the project was used

### Step 2: Pull and inspect CAS objects

When a thread errors, pull the `runtime_outputs_bundle_hash` to see what happened:

```
parameters={"action": "pull", "hashes": ["<runtime_outputs_bundle_hash>"]}
```

Then read the bundle JSON locally:

```
cat .ai/state/objects/objects/<ab>/<cd>/<full_hash>.json | python3 -m json.tool
```

The bundle lists all output files with their blob hashes. Key files:

- **Graph transcript**: `.ai/state/threads/<run_id>/transcript.jsonl` — step-by-step events
- **Graph knowledge**: `.ai/knowledge/agent/graphs/<graph_id>/<run_id>.md` — visual status table
- **Thread knowledge**: `.ai/knowledge/agent/threads/<path>/<thread_id>.md` — for forked directives
- **Execution snapshot**: referenced by `execution_snapshot_hash` in the bundle

### Step 3: Pull individual files

Pull specific blob hashes from the bundle to read their content:

```
parameters={"action": "pull", "hashes": ["<blob_hash_1>", "<blob_hash_2>"]}
```

Then read locally from `.ai/state/objects/blobs/<ab>/<cd>/<hash>`.

### Step 4: Read the graph transcript

For graph tools, the transcript JSONL contains every step with:

- `step_started` — which node started, what action
- `step_completed` — status, elapsed time, next node, result hash
- `graph_error` — full traceback if the graph crashed
- `state_checkpoint` — state snapshot at step boundaries

Parse events with:

```bash
cat .ai/state/objects/blobs/<hash> | python3 -c "
import sys, json
for line in sys.stdin:
    print(json.dumps(json.loads(line), indent=2))
"
```

### Step 5: Read the graph knowledge markdown

The signed knowledge markdown (`.md` in the bundle) has a visual table showing all nodes:

```
| #   | Node        | Status | Duration | Action                   | Details    |
| --- | ----------- | ------ | -------- | ------------------------ | ---------- |
| 1   | route       | ✅     | 0.3s     | rye/email/router         | ...        |
| 2   | draft_reply | ✅     | 10.2s    | rye/email/draft_response | ...        |
| 3   | send_reply  | ❌     | 0.5s     | rye/email/send           | error: ... |
```

## Execution Flow (What Happens on Remote)

1. **Auth** — bearer API key OR webhook HMAC
2. **Resolve HEAD** — load project snapshot from `project_refs` table
3. **Checkout** — create mutable execution space from cached snapshot
4. **User space** — mount user-space items (cross-project personal items)
5. **Secrets** — inject user secrets as env vars
6. **Execute** — run the tool/directive/graph via `ExecuteTool`
7. **Promote** — copy execution-local CAS objects to user CAS
8. **Ingest** — bundle runtime outputs (transcripts, knowledge) into CAS
9. **Fold-back** — merge execution manifest into HEAD:
   - Fast-forward if HEAD unchanged
   - Three-way merge if HEAD moved (bounded retry with jitter)
   - Conflict record stored on thread if unresolvable
10. **Cleanup** — remove mutable execution space

## Graph Execution on Remote

State graph tools (like `handle_inbound.yaml`) are executed by `walker.py`.

**Directives in graphs**: The walker automatically upgrades `thread="inline"` to `thread="fork"` when `item_type == "directive"`. This ensures an LLM thread is spawned — the walker has no LLM, so inline would return raw `your_directions` with no one to follow them. Explicit remote routing (`thread="remote:*"`) is preserved.

**Result structure for directives**: When a graph node executes a directive via fork, the result structure is:

```json
{
  "status": "completed",
  "type": "directive",
  "directive": "rye/email/draft_response",
  "thread_id": "rye/email/draft_response/draft_response-123456",
  "outputs": {
    "draft_body": "...",
    "draft_subject": "..."
  },
  "result": "..."
}
```

Graph `assign` mappings must reference `${result.outputs.<field>}` for directive outputs, not `${result.<field>}`. Tool results are flat — `${result.<field>}` works directly.

## Webhook Execution

External services trigger remote execution via webhooks. The binding controls what executes:

```
POST /execute
Headers:
  X-Webhook-Timestamp: <unix_timestamp>
  X-Webhook-Signature: <hmac_sha256>
  X-Webhook-Delivery-Id: <unique_id>
Body:
  {"hook_id": "wh_...", "parameters": {...}}
```

The binding (created via `POST /webhook-bindings`) locks down `item_type`, `item_id`, and `project_path`. The caller can only provide `parameters`. HMAC verification + replay protection prevents unauthorized execution.

## Configuration

Remotes are declared in `.ai/config/remotes/remotes.yaml`:

```yaml
remotes:
  default:
    url: "https://ryeos--ryeos-node-remote-server.modal.run"
    key_env: "RYE_REMOTE_API_KEY"
```

The `key_env` field names the environment variable holding the API key — the key itself is never stored in config.
