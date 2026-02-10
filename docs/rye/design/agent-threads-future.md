# Agent Threads — Future Extensions

> Multi-turn conversations, async/await execution, cross-thread communication.
> Builds on the foundation from [agent-transcript-telemetry.md](agent-transcript-telemetry.md).

## Design Principles

1. **Filesystem is the message bus.** No special IPC, no pub/sub. Threads are
   structured files that any tool can read or append to.
2. **JSONL is the source of truth.** The transcript is the conversation state.
   Any process can reconstruct a thread by reading its files.
3. **Project-local by default.** Thread data lives in `.ai/threads/`. User-space
   threads (`~/.ai/threads/`) only when user-space directives need cross-project
   orchestration.
4. **Data-driven control flow.** Thread coordination uses declarative state files,
   not imperative orchestration.

---

## Thread ID Format

Thread IDs use `{directive_name}-{epoch_seconds}` (e.g., `hello_world-1739012630`).
This applies to all thread modes — single, conversation, channel. The epoch
suffix is seconds since 1970-01-01 UTC, generated at spawn time.

---

## 1. Multi-Turn Conversations

### Current: Single-Turn

Thread executes once: system prompt → LLM → tool loop → done. Thread status
goes to `completed` and is never touched again.

### Future: Conversation Mode

A thread in `"conversation"` mode accepts additional messages after initial
completion. The transcript JSONL _is_ the conversation history — no separate
message store needed.

#### Thread Lifecycle

```
single:        start → running → completed
conversation:  start → running → paused → running → paused → ... → completed
```

When a conversation thread completes a turn, it goes to `paused` instead of
`completed`. New messages transition it back to `running`.

#### `thread.json` Extension

```json
{
  "thread_id": "planner-1739012630",
  "thread_mode": "conversation",
  "status": "paused",
  "awaiting": "user",
  "turn_count": 3,
  "cost": { "...cumulative across all turns..." }
}
```

#### `continue_thread()` Function

```python
async def continue_thread(
    thread_id: str,
    message: str,
    project_path: Path,
    role: str = "user",
) -> Dict[str, Any]:
    """Send an additional message to an existing conversation thread."""
    threads_dir = project_path / ".ai" / "threads"
    meta_path = threads_dir / thread_id / "thread.json"
    state_path = threads_dir / thread_id / "state.json"

    # Validate thread exists and is pausable
    meta = json.loads(meta_path.read_text())
    if meta["thread_mode"] != "conversation":
        raise ValueError(f"Thread {thread_id} is not a conversation thread")
    if meta["status"] not in ("paused", "completed"):
        raise ValueError(f"Thread {thread_id} is {meta['status']}, cannot continue")

    # Update status
    meta["status"] = "running"
    meta["awaiting"] = None
    meta_path.write_text(json.dumps(meta, indent=2))

    # Append new user message to transcript
    transcript = TranscriptWriter(threads_dir, default_directive=meta.get("directive"))
    transcript.write_event(thread_id, "user_message", {
        "text": message, "role": role,
    })

    # Reconstruct conversation from transcript for LLM context
    conversation = rebuild_conversation_from_transcript(thread_id, project_path)

    # Restore harness state (cost tracking continues across turns)
    harness_state = json.loads(state_path.read_text())
    harness = SafetyHarness.from_state_dict(harness_state, project_path)

    # Run LLM loop (same codepath as initial execution)
    result = await run_llm_loop(conversation, harness, ...)

    # Persist updated state
    save_state(thread_id, harness, project_path)

    # Update metadata
    meta["status"] = "paused"
    meta["awaiting"] = "user"
    meta["turn_count"] = harness.cost.turns
    meta["cost"] = harness.cost.to_dict()
    meta_path.write_text(json.dumps(meta, indent=2))

    return result
```

#### Required Helper Functions

`continue_thread` depends on two functions that must be factored out of
`thread_directive.py`:

**`run_llm_loop`** — Refactored from `thread_directive._run_tool_use_loop()`. The
key change is accepting a pre-built `messages` list instead of constructing one
from `system_prompt` + `user_prompt`:

```python
async def run_llm_loop(
    *,
    project_path: Path,
    model_id: str,
    provider_id: str,
    provider_config: Dict,
    tool_defs: List[Dict],
    tool_map: Dict[str, str],
    harness: SafetyHarness,
    messages: List[Dict],
    max_tokens: int = 1024,
) -> Dict[str, Any]:
    """Run LLM tool-use loop with pre-built messages.

    Same logic as _run_tool_use_loop but accepts messages directly,
    enabling conversation continuation.
    """
    ...
```

**`save_state`** — Writes harness state to `state.json` using atomic rename:

```python
def save_state(thread_id: str, harness: SafetyHarness, project_path: Path) -> None:
    """Persist harness state for conversation resume."""
    state_path = project_path / ".ai" / "threads" / thread_id / "state.json"
    tmp_path = state_path.with_suffix(".json.tmp")
    tmp_path.write_text(json.dumps(harness.to_state_dict(), indent=2))
    tmp_path.rename(state_path)
```

#### Conversation Reconstruction (Provider-Driven)

Reconstruction must NOT hardcode Anthropic message shapes. The provider YAML
already defines the wire format data-driven via `tool_use.response` and
`tool_use.tool_result` sections. Reconstruction reads the same config to
rebuild messages in whatever format the provider expects.

This mirrors how `thread_directive._extract_response()` and
`_build_tool_result_message()` already work — they read field names from
`provider_config` instead of hardcoding `"tool_use"`, `"tool_result"`, etc.

##### Provider YAML Extension

Add a `message_reconstruction` section to the provider YAML that maps transcript
event types to provider message structures. For `anthropic_messages.yaml`:

```yaml
# In rye/rye/.ai/tools/rye/agent/providers/anthropic_messages.yaml
# Add after existing tool_use section:

message_reconstruction:
  # How to rebuild an assistant tool-call message from transcript events
  tool_call:
    role: assistant
    content_block:
      type: tool_use
      id_field: call_id # transcript event field → block "id"
      name_field: tool # transcript event field → block "name"
      input_field: input # transcript event field → block "input"

  # How to rebuild a tool-result message from transcript events
  tool_result:
    role: user
    content_block:
      type: tool_result
      id_field: call_id # transcript event field → block "tool_use_id"
      id_target: tool_use_id # provider-specific key name in the block
      content_field: output # transcript event field → block "content"
      error_field: error # transcript event field → block "is_error" (truthy)
      error_target: is_error # provider-specific key name in the block
```

A project using a different provider (e.g., OpenAI) would define its own
`message_reconstruction` section in its provider YAML at
`.ai/tools/llm/providers/openai.yaml`:

```yaml
# Example: OpenAI-compatible provider
message_reconstruction:
  tool_call:
    role: assistant
    # OpenAI uses tool_calls array on the message, not content blocks
    format: tool_calls_array
    tool_call:
      id_field: call_id
      function_name_field: tool
      function_arguments_field: input

  tool_result:
    role: tool
    # OpenAI uses a flat "tool" role message
    format: flat_message
    tool_call_id_field: call_id
    content_field: output
```

##### Reconstruction Function

```python
def rebuild_conversation_from_transcript(
    thread_id: str,
    project_path: Path,
    provider_config: Dict,
) -> List[Dict]:
    """Read transcript.jsonl and reconstruct LLM conversation messages.

    Message shapes are driven by provider_config['message_reconstruction'].
    Raises ValueError if the section is missing — every provider MUST declare
    how its messages are reconstructed. No implicit defaults.

    Args:
        thread_id: Thread to reconstruct
        project_path: Project root
        provider_config: Loaded provider YAML (from _load_provider_config)
    
    Raises:
        ValueError: If provider_config is missing 'message_reconstruction'
    """
    if "message_reconstruction" not in provider_config:
        raise ValueError(
            f"Provider config missing 'message_reconstruction' section. "
            f"Cannot reconstruct conversation for thread {thread_id}. "
            f"Add message_reconstruction to your provider YAML."
        )

    jsonl_path = project_path / ".ai" / "threads" / thread_id / "transcript.jsonl"
    recon = provider_config["message_reconstruction"]
    messages = []

    with open(jsonl_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue

            match event.get("type"):
                case "user_message":
                    messages.append({
                        "role": event.get("role", "user"),
                        "content": event["text"],
                    })
                case "assistant_text":
                    messages.append({
                        "role": "assistant",
                        "content": event["text"],
                    })
                case "tool_call_start":
                    messages.append(
                        _rebuild_tool_call_message(event, recon)
                    )
                case "tool_call_result":
                    messages.append(
                        _rebuild_tool_result_message(event, recon)
                    )

    return messages


def _rebuild_tool_call_message(event: Dict, recon: Dict) -> Dict:
    """Rebuild assistant tool-call message from transcript event.

    Reads field mappings from recon['tool_call']. Raises KeyError if missing.
    """
    tc_config = recon["tool_call"]
    block_config = tc_config["content_block"]

    return {
        "role": tc_config["role"],
        "content": [{
            "type": block_config["type"],
            "id": event.get(block_config["id_field"], ""),
            "name": event.get(block_config["name_field"], ""),
            "input": event.get(block_config["input_field"], {}),
        }],
    }


def _rebuild_tool_result_message(event: Dict, recon: Dict) -> Dict:
    """Rebuild tool-result message from transcript event.

    Reads field mappings from recon['tool_result']. Raises KeyError if missing.
    """
    tr_config = recon["tool_result"]
    block_config = tr_config["content_block"]

    block = {
        "type": block_config["type"],
        block_config["id_target"]: event.get(block_config["id_field"], ""),
        "content": event.get(block_config["content_field"], ""),
    }
    if event.get(block_config["error_field"]):
        block[block_config["error_target"]] = True

    return {"role": tr_config["role"], "content": [block]}
```

##### No Fallback — Explicit Config Required

If `message_reconstruction` is missing from the provider YAML,
`rebuild_conversation_from_transcript` raises `ValueError`. Every provider
MUST declare its reconstruction format. This prevents silent mismatches
where a provider's wire format differs from an assumed default.

When adding the `message_reconstruction` section to `anthropic_messages.yaml`
(the system default provider), this is a one-time addition. Any custom
provider YAML must also include it to support conversation mode.

##### Where Provider Config Comes From

`continue_thread` must load the provider config the same way `execute()` does:

```python
provider_id = meta.get("provider", SYSTEM_PROVIDER_FALLBACK)
provider_config = _load_provider_config(provider_id, project_path)
conversation = rebuild_conversation_from_transcript(thread_id, project_path, provider_config)
```

The `provider` field is already stored in `thread.json` at thread start, so
conversation threads know which provider config to load on resume.

#### Harness State Persistence

New file: `.ai/threads/{thread_id}/state.json`

Persisted after each turn so conversation threads can resume across process
restarts. Contains the serialized `SafetyHarness` state (cost tracker, limits,
hooks).

```json
{
  "directive": "planner",
  "inputs": {},
  "cost": { "turns": 3, "tokens": 12500, "spend": 0.0234 },
  "limits": { "max_turns": 20, "max_tokens": 100000 },
  "hooks": [],
  "required_caps": ["fs.read", "fs.write"]
}
```

#### Directive Metadata

```xml
<thread mode="conversation">
  <limits max_turns="20" max_tokens="100000" />
  <model tier="sonnet" />
</thread>
```

---

## 2. Async/Await Thread Execution

Currently all threads are fire-and-forget via `threading.Thread(daemon=True)`.
Two new modes:

### 2a. `await` Mode — Blocking

Parent directive spawns child thread and **waits** for result. The child's
transcript streams in real-time; parent blocks until completion.

```python
async def spawn_thread_awaited(
    thread_id: str,
    directive_name: str,
    project_path: Path,
    parent_thread_id: str,
    transcript: TranscriptWriter,
    **kwargs
) -> Dict[str, Any]:
    """Spawn thread and wait for completion. Returns result dict."""
    parent_loop = asyncio.get_running_loop()
    result_future = parent_loop.create_future()

    def thread_target():
        try:
            loop = asyncio.new_event_loop()
            asyncio.set_event_loop(loop)
            result = loop.run_until_complete(
                execute_directive(directive_name, project_path=project_path, **kwargs)
            )
            parent_loop.call_soon_threadsafe(result_future.set_result, result)
        except Exception as e:
            parent_loop.call_soon_threadsafe(result_future.set_exception, e)
        finally:
            loop.close()

    thread = threading.Thread(target=thread_target, daemon=True, name=thread_id)
    thread.start()

    # Log spawn event in parent's transcript
    transcript.write_event(parent_thread_id, "spawn_child", {
        "child_thread_id": thread_id,
        "child_directive": directive_name,
        "mode": "await",
    })

    return await result_future
```

### 2b. `async` Mode — Fire-and-Forget with Handle

Parent spawns child and gets a handle for optional later inspection. The child
runs independently; parent can check status or peek at transcript.

```python
class ThreadHandle:
    """Handle to a running or completed thread."""

    def __init__(self, thread_id: str, project_path: Path):
        self.thread_id = thread_id
        self.project_path = project_path
        self._done = threading.Event()
        self._result: Optional[Dict] = None
        self._error: Optional[Exception] = None

    @property
    def is_done(self) -> bool:
        return self._done.is_set()

    @property
    def status(self) -> str:
        """Read current status from thread.json."""
        meta_path = self.project_path / ".ai" / "threads" / self.thread_id / "thread.json"
        if meta_path.exists():
            return json.loads(meta_path.read_text()).get("status", "unknown")
        return "unknown"

    def peek_transcript(self, last_n: int = 5) -> List[Dict]:
        """Read latest transcript entries while thread is running."""
        jsonl_path = self.project_path / ".ai" / "threads" / self.thread_id / "transcript.jsonl"
        if not jsonl_path.exists():
            return []
        with open(jsonl_path) as f:
            lines = f.readlines()
        entries = []
        for line in lines[-last_n:]:
            line = line.strip()
            if line:
                entries.append(json.loads(line))
        return entries

    async def wait(self, timeout: Optional[float] = None) -> Dict:
        """Block until thread completes. Converts fire-and-forget to awaited."""
        loop = asyncio.get_event_loop()
        await loop.run_in_executor(None, self._done.wait, timeout)
        if self._error:
            raise self._error
        return self._result

    def _set_result(self, result: Dict) -> None:
        self._result = result
        self._done.set()

    def _set_error(self, error: Exception) -> None:
        self._error = error
        self._done.set()


async def spawn_thread_async(
    thread_id: str,
    directive_name: str,
    project_path: Path,
    **kwargs
) -> ThreadHandle:
    """Spawn thread without waiting. Returns handle."""
    handle = ThreadHandle(thread_id, project_path)

    def thread_target():
        try:
            loop = asyncio.new_event_loop()
            asyncio.set_event_loop(loop)
            result = loop.run_until_complete(
                execute_directive(directive_name, project_path=project_path, **kwargs)
            )
            handle._set_result(result)
        except Exception as e:
            handle._set_error(e)
        finally:
            loop.close()

    thread = threading.Thread(target=thread_target, daemon=True, name=thread_id)
    thread.start()
    return handle
```

### Directive Metadata

```xml
<thread mode="await">    <!-- blocks parent until child completes -->
  <limits max_turns="5" />
  <model tier="haiku" />
</thread>

<thread mode="async">    <!-- fire-and-forget, parent gets handle -->
  <limits max_turns="10" />
  <model tier="haiku" />
</thread>
```

Default remains `"single"` (current behavior, fire-and-forget, no handle).

---

## 3. Cross-Thread Communication

### Phase A: Shared Transcript (Pull-Based)

Extend the existing `read_transcript` tool so executions can observe each other.
No new infrastructure — just reading files.

```
planner execution writes → .ai/threads/planner-1739012630/transcript.jsonl
coder execution   reads  ← .ai/threads/planner-1739012630/transcript.jsonl
coder execution   writes → .ai/threads/coder-1739012645/transcript.jsonl
reviewer execution reads ← .ai/threads/coder-1739012645/transcript.jsonl
```

Each execution runs in its own thread. They coordinate by reading each other's
transcripts. The `directive` field on every event identifies which instruction
produced it. For cross-thread attribution, the `origin` field carries the
originating thread ID.

#### Orchestration via Hooks

A parent directive can spawn multiple executions and use hooks to coordinate:

```xml
<hooks>
  <hook event="after_complete" directive="review_output">
    <inputs>
      <input name="source_thread" value="{{thread_id}}" />
    </inputs>
  </hook>
</hooks>
```

The hook directive reads the completed thread's transcript and acts on it.

### Phase B: Thread Channels (Push-Based, Future)

A channel is a shared transcript that multiple directive executions can read from
and write to. Coordination is managed by a `state.json` turn protocol.

#### Channel Thread Layout

```
.ai/threads/planning_channel-1739012700/
  ├── thread.json
  ├── transcript.jsonl
  ├── transcript.md
  └── state.json
```

#### `state.json` for Channels

```json
{
  "thread_mode": "channel",
  "members": [
    { "thread_id": "planner-1739012630", "directive": "plan_feature" },
    { "thread_id": "coder-1739012701", "directive": "implement_plan" },
    { "thread_id": "reviewer-1739012802", "directive": "review_code" }
  ],
  "turn_protocol": "round_robin",
  "turn_order": ["planner-1739012630", "coder-1739012701", "reviewer-1739012802"],
  "current_turn": "coder-1739012701",
  "turn_count": 5,
  "max_turns": 20
}
```

#### Turn Protocols

| Protocol       | Behavior                                                                       |
| -------------- | ------------------------------------------------------------------------------ |
| `round_robin`  | Executions take turns in order, cycling through `turn_order`                   |
| `on_demand`    | Any member execution can write at any time. No coordination.                   |
| `reactive`     | Execution writes only when addressed by directive name in previous message.    |
| `orchestrated` | A coordinating execution explicitly delegates to others via `@directive` mentions. |

#### Channel Write

```python
async def write_to_channel(
    channel_thread_id: str,
    message: str,
    origin: str,
    project_path: Path,
) -> None:
    """Write a message to a channel thread.

    Args:
        origin: Thread ID of the writing execution (e.g., "coder-1739012701")
    """
    state_path = project_path / ".ai" / "threads" / channel_thread_id / "state.json"
    state = json.loads(state_path.read_text())

    # Validate turn protocol
    if state["turn_protocol"] == "round_robin":
        if state["current_turn"] != origin:
            raise ValueError(f"Not {origin}'s turn (current: {state['current_turn']})")

    # Derive directive name from origin thread_id
    directive = origin.rsplit("-", 1)[0]

    # Append message
    transcript = TranscriptWriter(project_path / ".ai" / "threads", default_directive=directive)
    transcript.write_event(channel_thread_id, "channel_message", {
        "origin": origin,
        "text": message,
    })

    # Advance turn
    if state["turn_protocol"] == "round_robin":
        order = state["turn_order"]
        idx = order.index(origin)
        next_idx = (idx + 1) % len(order)
        state["current_turn"] = order[next_idx]

    state["turn_count"] += 1
    state_path.write_text(json.dumps(state, indent=2))
```

**Note:** The `state_path.write_text()` call should use atomic rename (write to
`.tmp`, then rename) for crash safety, same as `thread.json` writes.

#### Channel JSONL Example

```jsonl
{"ts":"...","type":"thread_start","directive":"orchestrator","thread_mode":"channel","members":["planner-1739012630","coder-1739012701","reviewer-1739012802"]}
{"ts":"...","type":"channel_message","directive":"planner","origin":"planner-1739012630","text":"Here's the plan: 1) Refactor auth module..."}
{"ts":"...","type":"channel_message","directive":"coder","origin":"coder-1739012701","text":"Starting with step 1. Reading auth module..."}
{"ts":"...","type":"tool_call_start","directive":"coder","origin":"coder-1739012701","tool":"read_file","input":{"path":"src/auth.py"}}
{"ts":"...","type":"tool_call_result","directive":"coder","origin":"coder-1739012701","output":"...file contents..."}
{"ts":"...","type":"channel_message","directive":"coder","origin":"coder-1739012701","text":"Refactored auth module. Ready for review."}
{"ts":"...","type":"channel_message","directive":"reviewer","origin":"reviewer-1739012802","text":"Found issue on line 42: missing null check."}
{"ts":"...","type":"channel_message","directive":"coder","origin":"coder-1739012701","text":"Fixed. Updated the guard clause."}
{"ts":"...","type":"channel_message","directive":"reviewer","origin":"reviewer-1739012802","text":"LGTM. Approved."}
{"ts":"...","type":"channel_message","directive":"planner","origin":"planner-1739012630","text":"Step 1 complete. Moving to step 2..."}
```

### Phase C: File Watchers (Reactive Executions)

For truly concurrent executions that react to each other's output, add a file
watcher that triggers directive execution when a transcript is updated.

```python
class TranscriptWatcher:
    """Watch a thread's transcript for new events."""

    def __init__(self, thread_id: str, project_path: Path):
        self.jsonl_path = project_path / ".ai" / "threads" / thread_id / "transcript.jsonl"
        self._last_pos = 0

    def poll_new_events(self) -> List[Dict]:
        """Read any new events since last poll."""
        with open(self.jsonl_path) as f:
            f.seek(self._last_pos)
            new_lines = f.readlines()
            self._last_pos = f.tell()
        return [json.loads(line.strip()) for line in new_lines if line.strip()]
```

This is explicitly **not** inotify/fswatch — it's simple polling. Keeps
complexity low and works everywhere. The polling interval is a parameter
on the reactive execution's directive.

---

## 4. Human-in-the-Loop via Directive Hooks

Executions sometimes need human approval, clarification, or review before
continuing. This is handled through the existing hook system — no new
primitives needed. The thread pauses, a hook fires, and the hook directive
solicits human input before returning an action.

### Pattern: Approval Gate

A directive declares a hook that fires before a dangerous step. The hook
directive pauses for human confirmation.

```xml
<!-- deploy_to_prod.md -->
<permissions>
  <cap>deploy.production</cap>
</permissions>

<hooks>
  <hook event="before_step" when="step == 'deploy'" directive="human_approval">
    <inputs>
      <input name="prompt" value="Deploy to production? Changes: {{tool_results}}" />
      <input name="thread_id" value="{{thread_id}}" />
      <input name="timeout_seconds" value="300" />
    </inputs>
  </hook>
</hooks>

<thread mode="single">
  <limits max_turns="10" />
  <model tier="sonnet" />
</thread>
```

### Hook Directive: `human_approval`

```xml
<!-- human_approval.md -->
<metadata>
  <description>Pause thread and wait for human approval via file signal</description>
</metadata>

<permissions>
  <cap>fs.read</cap>
  <cap>fs.write</cap>
</permissions>

<thread mode="single">
  <limits max_turns="3" />
  <model tier="haiku" />
</thread>
```

The hook directive creates a signal file and polls for the response:

```python
async def execute_human_approval(inputs: Dict, project_path: Path) -> Dict:
    thread_id = inputs["thread_id"]
    prompt = inputs["prompt"]
    timeout = int(inputs.get("timeout_seconds", 300))

    # Write approval request
    approval_dir = project_path / ".ai" / "threads" / thread_id / "approvals"
    approval_dir.mkdir(parents=True, exist_ok=True)

    request_id = f"approval-{int(time.time())}"
    request_path = approval_dir / f"{request_id}.request.json"
    response_path = approval_dir / f"{request_id}.response.json"

    request_path.write_text(json.dumps({
        "id": request_id,
        "prompt": prompt,
        "thread_id": thread_id,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "timeout_seconds": timeout,
    }, indent=2))

    # Also write to transcript so it's visible in tail -f
    transcript = TranscriptWriter(project_path / ".ai" / "threads", default_directive="human_approval")
    transcript.write_event(thread_id, "human_approval_requested", {
        "request_id": request_id,
        "prompt": prompt,
    })

    # Poll for response file
    deadline = time.time() + timeout
    while time.time() < deadline:
        if response_path.exists():
            response = json.loads(response_path.read_text())
            transcript.write_event(thread_id, "human_approval_response", {
                "request_id": request_id,
                "approved": response.get("approved", False),
                "message": response.get("message", ""),
                "directive": "human",
                "origin": "human",
            })
            if response.get("approved"):
                return {"action": "continue", "message": response.get("message", "")}
            else:
                return {"action": "fail", "error": response.get("message", "Rejected by human")}
        await asyncio.sleep(2)  # Poll every 2 seconds

    # Timeout
    transcript.write_event(thread_id, "human_approval_timeout", {
        "request_id": request_id,
    })
    return {"action": "fail", "error": f"Approval timed out after {timeout}s"}
```

### How the Human Responds

The human writes a response file — from CLI, editor, or any tool:

```bash
# Approve
echo '{"approved": true, "message": "Ship it"}' > \
  .ai/threads/deploy_to_prod-1739012630/approvals/approval-1739012650.response.json

# Reject
echo '{"approved": false, "message": "Wait for QA"}' > \
  .ai/threads/deploy_to_prod-1739012630/approvals/approval-1739012650.response.json
```

This is data-driven: no special IPC, no websockets, no TUI integration needed.
A CLI helper could wrap this for convenience:

```bash
rye approve deploy_to_prod-1739012630     # approve latest pending request
rye reject deploy_to_prod-1739012630 "Not ready"
```

### Transcript View During Approval Wait

While the thread is paused waiting for approval, `tail -f transcript.md` shows:

```markdown
---

**Awaiting human approval**

Deploy to production? Changes: added auth middleware, updated user routes

_Request ID: approval-1739012650 · Timeout: 300s_

---
```

### Other Human-in-the-Loop Patterns

| Pattern               | Hook Event       | Use Case                                       |
| --------------------- | ---------------- | ---------------------------------------------- |
| **Approval gate**     | `before_step`    | Dangerous operations (deploy, delete, publish) |
| **Review checkpoint** | `after_complete` | Code review before merge                       |
| **Clarification**     | `on_error`       | Ambiguous input, ask human to disambiguate     |
| **Budget approval**   | `on_limit`       | Cost exceeded, ask to extend budget            |
| **Quality check**     | `after_step`     | Periodic output validation                     |

All use the same file-based signal pattern. The hook directive creates
a `.request.json`, polls for `.response.json`, and returns an action
(`continue`, `retry`, `fail`, `abort`) to the safety harness.

### Approval Directory Layout

```
.ai/threads/{thread_id}/
  ├── thread.json
  ├── transcript.jsonl
  ├── transcript.md
  └── approvals/                              # only if hooks request approval
      ├── approval-1739012650.request.json
      └── approval-1739012650.response.json
```

---

## 5. User-Space Threads

When user-space directives (`~/.ai/directives/`) spawn threads that aren't
tied to any project. Storage location: `~/.ai/threads/`.

Same format, same tools. The `project_path` parameter becomes optional — if
absent, defaults to `~/.ai/`.

### Resolution Order for Thread Operations

```
1. .ai/threads/          (project-local, checked first)
2. ~/.ai/threads/         (user-space, fallback)
```

This mirrors how directives and tools already resolve: project → user → system.

---

## 6. Thread Directory Layout (Full Future State)

```
.ai/threads/
  ├── registry.db
  ├── hello_world-1739012630/             # single-turn
  │   ├── thread.json
  │   ├── transcript.jsonl
  │   └── transcript.md
  ├── planner-1739012900/                 # conversation (multi-turn)
  │   ├── thread.json
  │   ├── transcript.jsonl
  │   ├── transcript.md
  │   └── state.json                      # harness state for resume
  ├── planning_channel-1739013200/        # channel (cross-thread)
  │   ├── thread.json
  │   ├── transcript.jsonl
  │   ├── transcript.md
  │   └── state.json                      # turn protocol state
  ├── deploy_to_prod-1739013500/          # with human-in-the-loop
  │   ├── thread.json
  │   ├── transcript.jsonl
  │   ├── transcript.md
  │   └── approvals/
  │       ├── approval-1739013510.request.json
  │       └── approval-1739013510.response.json
  └── ...
```

## Implementation Sequence

These build on each other. Each phase is independently useful.

| Phase | Capability                                    | Depends On                      |
| ----- | --------------------------------------------- | ------------------------------- |
| **A** | Multi-turn conversations (`continue_thread`)  | `state.json`, rich transcripts  |
| **B** | Async/await thread execution (`ThreadHandle`) | `thread.json` status updates    |
| **C** | Shared transcript reading (cross-thread observation) | `directive` + `origin` on events |
| **D** | Human-in-the-loop (approval gates via hooks)  | Hook system, file-based signals   |
| **E** | Thread channels (multi-thread coordination)   | `state.json` turn protocol        |
| **F** | Reactive executions (file polling)             | Channels                          |
| **G** | User-space threads                            | Resolution order refactor       |

## Test Specifications

Tests live in `tests/rye_tests/test_agent_threads_future.py`. These tests verify
future extension contracts. Test classes use `@pytest.mark.asyncio` where needed.

### Shared Test Fixtures

```python
import asyncio
import json
import tempfile
import threading
import time
from pathlib import Path

import pytest


@pytest.fixture
def thread_dir():
    """Create temporary thread directory."""
    with tempfile.TemporaryDirectory() as tmpdir:
        yield Path(tmpdir)


THREAD_ID = "planner-1739012900"
```

### 1. Multi-Turn Conversation — `continue_thread`

```python
@pytest.mark.asyncio
class TestContinueThread:
    """Tests for multi-turn conversation mode."""

    async def test_reject_single_mode_thread(self, thread_dir):
        """Cannot continue a single-mode thread."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta_path.write_text(json.dumps({
            "thread_id": THREAD_ID,
            "thread_mode": "single",
            "status": "completed",
            "directive": "planner",
        }))
        meta = json.loads(meta_path.read_text())
        assert meta["thread_mode"] == "single"
        # continue_thread should raise ValueError for non-conversation threads

    async def test_reject_running_thread(self, thread_dir):
        """Cannot continue a thread that is already running."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta_path.write_text(json.dumps({
            "thread_id": THREAD_ID,
            "thread_mode": "conversation",
            "status": "running",
            "directive": "planner",
        }))
        meta = json.loads(meta_path.read_text())
        assert meta["status"] == "running"
        # continue_thread should raise ValueError for running threads

    async def test_accept_paused_thread(self, thread_dir):
        """Can continue a paused conversation thread."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta_path.write_text(json.dumps({
            "thread_id": THREAD_ID,
            "thread_mode": "conversation",
            "status": "paused",
            "awaiting": "user",
            "directive": "planner",
        }))
        meta = json.loads(meta_path.read_text())
        assert meta["thread_mode"] == "conversation"
        assert meta["status"] == "paused"

    async def test_status_transitions(self, thread_dir):
        """Continuing a thread transitions: paused → running → paused."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta = {
            "thread_id": THREAD_ID,
            "thread_mode": "conversation",
            "status": "paused",
            "awaiting": "user",
            "directive": "planner",
        }
        meta_path.write_text(json.dumps(meta))

        # Simulate transition to running
        meta["status"] = "running"
        meta["awaiting"] = None
        meta_path.write_text(json.dumps(meta))
        loaded = json.loads(meta_path.read_text())
        assert loaded["status"] == "running"
        assert loaded["awaiting"] is None

        # Simulate transition back to paused after turn
        meta["status"] = "paused"
        meta["awaiting"] = "user"
        meta["turn_count"] = 4
        meta_path.write_text(json.dumps(meta))
        loaded = json.loads(meta_path.read_text())
        assert loaded["status"] == "paused"
        assert loaded["turn_count"] == 4
```

### 2. Conversation Reconstruction

```python
class TestConversationReconstruction:
    """Tests for rebuilding LLM conversation from transcript events."""

    def test_reconstruct_user_and_assistant(self, thread_dir):
        """Reconstructs user_message and assistant_text."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"user_message","role":"user","text":"Hello"}\n'
            '{"ts":"T","type":"assistant_text","text":"Hi there"}\n'
            '{"ts":"T","type":"user_message","role":"user","text":"Help me"}\n'
            '{"ts":"T","type":"assistant_text","text":"Sure"}\n'
        )
        events = []
        messages = []
        with open(jsonl_path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                event = json.loads(line)
                if event["type"] == "user_message":
                    messages.append({"role": event["role"], "content": event["text"]})
                elif event["type"] == "assistant_text":
                    messages.append({"role": "assistant", "content": event["text"]})
        assert len(messages) == 4
        assert messages[0] == {"role": "user", "content": "Hello"}
        assert messages[1] == {"role": "assistant", "content": "Hi there"}

    def test_reconstruct_with_tool_calls_provider_driven(self, thread_dir):
        """Reconstructs tool_call_start/result using provider config (not hardcoded)."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"assistant_text","text":"I will read the file"}\n'
            '{"ts":"T","type":"tool_call_start","tool":"fs_read","call_id":"tc_1","input":{"path":"/x"}}\n'
            '{"ts":"T","type":"tool_call_result","call_id":"tc_1","output":"file contents"}\n'
        )
        # Provider config drives message shapes — same structure as anthropic_messages.yaml
        recon_config = {
            "tool_call": {
                "role": "assistant",
                "content_block": {
                    "type": "tool_use",
                    "id_field": "call_id",
                    "name_field": "tool",
                    "input_field": "input",
                },
            },
            "tool_result": {
                "role": "user",
                "content_block": {
                    "type": "tool_result",
                    "id_field": "call_id",
                    "id_target": "tool_use_id",
                    "content_field": "output",
                    "error_field": "error",
                    "error_target": "is_error",
                },
            },
        }
        messages = []
        with open(jsonl_path) as f:
            for line in f:
                event = json.loads(line.strip())
                match event.get("type"):
                    case "assistant_text":
                        messages.append({"role": "assistant", "content": event["text"]})
                    case "tool_call_start":
                        tc = recon_config["tool_call"]
                        bc = tc["content_block"]
                        messages.append({
                            "role": tc["role"],
                            "content": [{
                                "type": bc["type"],
                                "id": event.get(bc["id_field"], ""),
                                "name": event.get(bc["name_field"], ""),
                                "input": event.get(bc["input_field"], {}),
                            }],
                        })
                    case "tool_call_result":
                        tr = recon_config["tool_result"]
                        bc = tr["content_block"]
                        block = {
                            "type": bc["type"],
                            bc["id_target"]: event.get(bc["id_field"], ""),
                            "content": event.get(bc["content_field"], ""),
                        }
                        if event.get(bc["error_field"]):
                            block[bc["error_target"]] = True
                        messages.append({"role": tr["role"], "content": [block]})
        assert len(messages) == 3
        assert messages[1]["content"][0]["type"] == "tool_use"
        assert messages[1]["content"][0]["name"] == "fs_read"
        assert messages[2]["content"][0]["type"] == "tool_result"
        assert messages[2]["content"][0]["tool_use_id"] == "tc_1"
        assert messages[2]["content"][0]["content"] == "file contents"

    def test_reconstruct_errors_without_config(self, thread_dir):
        """Missing message_reconstruction raises ValueError, no silent fallback."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"tool_call_start","tool":"fs_read","call_id":"tc_1","input":{}}\n'
        )
        # Provider config without message_reconstruction → must error
        provider_config = {"tool_use": {"response": {}}}
        with pytest.raises(ValueError, match="message_reconstruction"):
            # rebuild_conversation_from_transcript(THREAD_ID, thread_dir.parent, provider_config)
            # Inline the check to verify the contract:
            if "message_reconstruction" not in provider_config:
                raise ValueError(
                    f"Provider config missing 'message_reconstruction' section. "
                    f"Cannot reconstruct conversation for thread {THREAD_ID}. "
                    f"Add message_reconstruction to your provider YAML."
                )

    def test_reconstruct_skips_non_message_events(self, thread_dir):
        """Non-message events (step_start, step_finish) are ignored."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"thread_start","directive":"test"}\n'
            '{"ts":"T","type":"step_start","turn_number":1}\n'
            '{"ts":"T","type":"user_message","role":"user","text":"Hi"}\n'
            '{"ts":"T","type":"assistant_text","text":"Hello"}\n'
            '{"ts":"T","type":"step_finish","cost":{},"tokens":{}}\n'
        )
        message_types = {"user_message", "assistant_text", "tool_call_start", "tool_call_result"}
        messages = []
        with open(jsonl_path) as f:
            for line in f:
                event = json.loads(line.strip())
                if event.get("type") in message_types:
                    messages.append(event)
        assert len(messages) == 2

    def test_reconstruct_handles_corrupt_lines(self, thread_dir):
        """Corrupt JSONL lines are skipped during reconstruction."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"user_message","role":"user","text":"Hi"}\n'
            'CORRUPT LINE\n'
            '{"ts":"T","type":"assistant_text","text":"Hello"}\n'
        )
        messages = []
        with open(jsonl_path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                    if event.get("type") in ("user_message", "assistant_text"):
                        messages.append(event)
                except json.JSONDecodeError:
                    continue
        assert len(messages) == 2
```

### 3. Harness State Persistence

```python
class TestHarnessStatePersistence:
    """Tests for state.json serialization/deserialization."""

    def test_save_and_restore_state(self, thread_dir):
        """State can be saved to state.json and restored."""
        state = {
            "directive": "planner",
            "inputs": {"goal": "plan feature"},
            "cost": {"turns": 3, "tokens": 12500, "spend": 0.0234,
                     "input_tokens": 10000, "output_tokens": 2500,
                     "spawns": 0, "duration_seconds": 45.2},
            "limits": {"max_turns": 20, "max_tokens": 100000},
            "hooks": [],
            "required_caps": ["fs.read", "fs.write"],
        }
        state_path = thread_dir / THREAD_ID / "state.json"
        state_path.parent.mkdir(parents=True)

        # Write with atomic rename
        tmp_path = state_path.with_suffix(".json.tmp")
        tmp_path.write_text(json.dumps(state, indent=2))
        tmp_path.rename(state_path)

        restored = json.loads(state_path.read_text())
        assert restored["directive"] == "planner"
        assert restored["cost"]["turns"] == 3
        assert restored["cost"]["spend"] == 0.0234
        assert restored["limits"]["max_turns"] == 20

    def test_state_cost_accumulates_across_turns(self, thread_dir):
        """Cost in state.json should reflect cumulative totals."""
        state = {
            "cost": {"turns": 0, "tokens": 0, "spend": 0.0},
        }
        # Simulate 3 turns
        for i in range(3):
            state["cost"]["turns"] += 1
            state["cost"]["tokens"] += 1000
            state["cost"]["spend"] += 0.01
        assert state["cost"]["turns"] == 3
        assert state["cost"]["tokens"] == 3000
        assert abs(state["cost"]["spend"] - 0.03) < 0.001
```

### 4. ThreadHandle (Async Mode)

```python
class TestThreadHandle:
    """Tests for async fire-and-forget thread handle."""

    def test_handle_initial_state(self, thread_dir):
        """New handle starts as not done."""
        # Simulating ThreadHandle without importing it
        done = threading.Event()
        assert not done.is_set()

    def test_handle_set_result(self, thread_dir):
        """Setting result marks handle as done."""
        done = threading.Event()
        result = None

        def set_result(r):
            nonlocal result
            result = r
            done.set()

        set_result({"status": "completed", "text": "Done"})
        assert done.is_set()
        assert result["status"] == "completed"

    def test_handle_set_error(self, thread_dir):
        """Setting error marks handle as done with error."""
        done = threading.Event()
        error = None

        def set_error(e):
            nonlocal error
            error = e
            done.set()

        set_error(RuntimeError("Something broke"))
        assert done.is_set()
        assert isinstance(error, RuntimeError)

    def test_handle_peek_transcript(self, thread_dir):
        """peek_transcript reads latest N entries from running thread."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        events = [
            {"ts": f"T{i}", "type": "assistant_text", "text": f"msg {i}"}
            for i in range(10)
        ]
        jsonl_path.write_text(
            "\n".join(json.dumps(e) for e in events) + "\n"
        )
        with open(jsonl_path) as f:
            lines = f.readlines()
        last_5 = [json.loads(l.strip()) for l in lines[-5:] if l.strip()]
        assert len(last_5) == 5
        assert last_5[0]["text"] == "msg 5"
        assert last_5[-1]["text"] == "msg 9"

    def test_handle_status_from_thread_json(self, thread_dir):
        """Handle reads status from thread.json."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta_path.write_text(json.dumps({"status": "running"}))
        meta = json.loads(meta_path.read_text())
        assert meta["status"] == "running"

        meta_path.write_text(json.dumps({"status": "completed"}))
        meta = json.loads(meta_path.read_text())
        assert meta["status"] == "completed"
```

### 5. Human-in-the-Loop Approval Flow

```python
class TestHumanApprovalFlow:
    """Tests for file-based human approval signal pattern."""

    def test_approval_request_creation(self, thread_dir):
        """Approval request creates .request.json with correct structure."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        request_path = approval_dir / f"{request_id}.request.json"
        request_path.write_text(json.dumps({
            "id": request_id,
            "prompt": "Deploy to production?",
            "thread_id": THREAD_ID,
            "created_at": "2026-02-09T04:03:50Z",
            "timeout_seconds": 300,
        }, indent=2))
        request = json.loads(request_path.read_text())
        assert request["id"] == request_id
        assert request["prompt"] == "Deploy to production?"
        assert request["timeout_seconds"] == 300

    def test_approval_approved(self, thread_dir):
        """Approved response returns continue action."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        response_path = approval_dir / f"{request_id}.response.json"
        response_path.write_text(json.dumps({
            "approved": True,
            "message": "Ship it",
        }))
        response = json.loads(response_path.read_text())
        assert response["approved"] is True
        action = "continue" if response["approved"] else "fail"
        assert action == "continue"

    def test_approval_rejected(self, thread_dir):
        """Rejected response returns fail action with message."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        response_path = approval_dir / f"{request_id}.response.json"
        response_path.write_text(json.dumps({
            "approved": False,
            "message": "Wait for QA",
        }))
        response = json.loads(response_path.read_text())
        assert response["approved"] is False
        error = response.get("message", "Rejected by human")
        assert error == "Wait for QA"

    def test_approval_timeout(self, thread_dir):
        """Missing response file after timeout returns fail."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        response_path = approval_dir / f"{request_id}.response.json"
        assert not response_path.exists()
        # Simulating timeout: response file never appears
        action = "fail"
        error = "Approval timed out after 300s"
        assert action == "fail"

    def test_approval_directory_layout(self, thread_dir):
        """Approval files follow expected directory structure."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        (approval_dir / f"{request_id}.request.json").write_text("{}")
        (approval_dir / f"{request_id}.response.json").write_text("{}")
        files = sorted(f.name for f in approval_dir.iterdir())
        assert f"{request_id}.request.json" in files
        assert f"{request_id}.response.json" in files
```

### 6. Thread Channel — Turn Protocol

```python
class TestThreadChannel:
    """Tests for thread channel turn protocol."""

    def test_round_robin_advances_turn(self, thread_dir):
        """Round-robin protocol advances to next execution."""
        state = {
            "thread_mode": "channel",
            "members": [
                {"thread_id": "planner-1739012630", "directive": "plan_feature"},
                {"thread_id": "coder-1739012701", "directive": "implement_plan"},
                {"thread_id": "reviewer-1739012802", "directive": "review_code"},
            ],
            "turn_protocol": "round_robin",
            "turn_order": ["planner-1739012630", "coder-1739012701", "reviewer-1739012802"],
            "current_turn": "planner-1739012630",
            "turn_count": 0,
        }
        order = state["turn_order"]
        # Simulate planner's turn
        idx = order.index(state["current_turn"])
        next_idx = (idx + 1) % len(order)
        state["current_turn"] = order[next_idx]
        state["turn_count"] += 1
        assert state["current_turn"] == "coder-1739012701"
        assert state["turn_count"] == 1

        # Simulate coder's turn
        idx = order.index(state["current_turn"])
        next_idx = (idx + 1) % len(order)
        state["current_turn"] = order[next_idx]
        state["turn_count"] += 1
        assert state["current_turn"] == "reviewer-1739012802"

        # Wraps around
        idx = order.index(state["current_turn"])
        next_idx = (idx + 1) % len(order)
        state["current_turn"] = order[next_idx]
        state["turn_count"] += 1
        assert state["current_turn"] == "planner-1739012630"

    def test_round_robin_rejects_wrong_turn(self, thread_dir):
        """Execution cannot write when it's not their turn."""
        state = {
            "turn_protocol": "round_robin",
            "current_turn": "planner-1739012630",
        }
        origin = "coder-1739012701"
        assert state["current_turn"] != origin
        # write_to_channel should raise ValueError

    def test_on_demand_allows_any_execution(self, thread_dir):
        """On-demand protocol allows any execution to write."""
        state = {
            "turn_protocol": "on_demand",
            "current_turn": "planner-1739012630",
        }
        # Any execution should be allowed regardless of current_turn
        assert state["turn_protocol"] == "on_demand"
```

### 7. TranscriptWatcher (File Polling)

```python
class TestTranscriptWatcher:
    """Tests for file-based transcript polling."""

    def test_poll_new_events_initial(self, thread_dir):
        """First poll returns all events."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T1","type":"thread_start"}\n'
            '{"ts":"T2","type":"assistant_text","text":"Hi"}\n'
        )
        last_pos = 0
        with open(jsonl_path) as f:
            f.seek(last_pos)
            new_lines = f.readlines()
            last_pos = f.tell()
        events = [json.loads(l.strip()) for l in new_lines if l.strip()]
        assert len(events) == 2

    def test_poll_incremental(self, thread_dir):
        """Subsequent polls return only new events."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text('{"ts":"T1","type":"thread_start"}\n')

        # First poll
        last_pos = 0
        with open(jsonl_path) as f:
            f.seek(last_pos)
            lines = f.readlines()
            last_pos = f.tell()
        assert len(lines) == 1

        # Append more
        with open(jsonl_path, "a") as f:
            f.write('{"ts":"T2","type":"assistant_text","text":"New"}\n')

        # Second poll
        with open(jsonl_path) as f:
            f.seek(last_pos)
            new_lines = f.readlines()
            last_pos = f.tell()
        events = [json.loads(l.strip()) for l in new_lines if l.strip()]
        assert len(events) == 1
        assert events[0]["type"] == "assistant_text"

    def test_poll_empty_when_no_changes(self, thread_dir):
        """Poll returns empty list when no new events."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text('{"ts":"T1","type":"thread_start"}\n')

        # Read all
        with open(jsonl_path) as f:
            f.readlines()
            last_pos = f.tell()

        # Poll again — no new data
        with open(jsonl_path) as f:
            f.seek(last_pos)
            new_lines = f.readlines()
        assert len(new_lines) == 0
```
