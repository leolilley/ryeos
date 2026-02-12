# Agent Threads — Phase 1-6 Implementation Complete

Complete implementation of multi-turn conversation, multi-agent coordination, and human-in-the-loop approval flows for Rye agent threads.

**Status:** ✅ All 79 tests passing

---

## Implementation Overview

This implementation follows the design from `docs/rye/design/agent-threads-future.md` with 6 phases:

| Phase | Component           | Status  | Tests |
| ----- | ------------------- | ------- | ----- |
| **1** | Core helpers        | ✅ Done | 11    |
| **2** | Conversation mode   | ✅ Done | 6     |
| **3** | Human approval flow | ✅ Done | 17    |
| **4** | Thread channels     | ✅ Done | 20    |
| **5** | TranscriptWatcher   | ✅ Done | 17    |
| **6** | Integration tests   | ✅ Done | 8     |

---

## Phase 1: Core Helpers

**File:** `core_helpers.py` (371 lines)

Extracted reusable functions for conversation continuation:

### `save_state(thread_id, harness, project_path)`

Persists harness state (cost, limits, permissions) to `.ai/threads/{thread_id}/state.json` using atomic tmp→rename pattern.

```python
harness = SafetyHarness(...)
save_state(thread_id, harness, project_path)
```

### `rebuild_conversation_from_transcript(thread_id, project_path, provider_config)`

Reconstructs LLM message history from `transcript.jsonl` using provider-specific configuration.

**Key feature:** Data-driven reconstruction (no Anthropic hardcoding)

- Provider YAML defines message field mappings
- Function reads `provider_config['message_reconstruction']` section
- Supports any provider (Anthropic, OpenAI, etc.)

```python
messages = rebuild_conversation_from_transcript(
    thread_id="planner-1739012630",
    project_path=Path("."),
    provider_config=provider_yaml,
)
```

### `run_llm_loop(*, project_path, model_id, provider_id, provider_config, tool_defs, tool_map, harness, messages, ...)`

Executes LLM with tool-use loop, accepting pre-built messages (enabling conversation continuation).

Refactored from `thread_directive._run_tool_use_loop` with:

- Pre-built messages input (instead of system/user prompts)
- Cost tracking via harness
- Transcript event emission
- Tool execution and result handling

**Tests:** `test_core_helpers.py` (11 tests)

---

## Phase 2: Conversation Mode

**File:** `conversation_mode.py` (379 lines)

Multi-turn conversation threads with pause/resume capability.

### Thread Lifecycle

```
Single-turn:        start → running → completed
Conversation:       start → running → paused → running → paused → ... → completed
```

### `continue_thread(thread_id, message, project_path, role="user")`

Resumes a paused conversation thread:

1. Validates thread is conversation mode and paused
2. Changes status: paused → running
3. Appends user message to transcript
4. Reconstructs full conversation history
5. Restores harness state (cost tracking continues)
6. Runs LLM loop with context
7. Persists updated state
8. Updates metadata (turn_count, cumulative cost)
9. Changes status back to paused

```python
result = await continue_thread(
    thread_id="planner-1739012630",
    message="How much will this cost?",
    project_path=Path("."),
)
# Returns:
# {
#     "success": True,
#     "status": "paused",
#     "turn_count": 2,
#     "cost": {"tokens": 250, "spend": 0.02, ...},
#     "text": "It will cost approximately...",
# }
```

### Thread Metadata Extension

```json
{
  "thread_id": "planner-1739012630",
  "thread_mode": "conversation",
  "status": "paused",
  "awaiting": "user",
  "turn_count": 3,
  "cost": {
    "tokens": 500,
    "spend": 0.05,
    "turns": 3,
    "duration_seconds": 45.2
  }
}
```

**Tests:** `test_conversation_mode.py` (6 tests)

---

## Phase 3: Human Approval Flow

**File:** `approval_flow.py` (306 lines)

File-based approval request/response pattern for deployment gates.

### Directory Structure

```
.ai/threads/{thread_id}/approvals/
  {request_id}.request.json       # Created by LLM/agent
  {request_id}.response.json      # Written by human/approver
```

### Classes

**`ApprovalRequest`**

```python
req = ApprovalRequest(
    request_id="approval-1739012650",
    prompt="Deploy to production?",
    thread_id="deploy-plan-123",
    timeout_seconds=300,
)
req.to_dict()  # Serialize to JSON
```

**`ApprovalResponse`**

```python
resp = ApprovalResponse(
    approved=True,
    message="Approved for production",
    request_id="approval-123",
)
```

### Functions

**`request_approval(thread_id, prompt, project_path, timeout_seconds=300)`**

- Creates `.request.json` file
- Returns request_id for polling

**`wait_for_approval(request_id, thread_id, project_path, timeout_seconds=None)`**

- Blocking poll for response
- Raises TimeoutError if timeout exceeded
- Returns response dict when available

**`poll_approval(request_id, thread_id, project_path)`**

- Non-blocking check for response
- Returns None if not yet answered

**`write_approval_response(request_id, thread_id, approved, message, project_path)`**

- Approvers/testers write response files

**`list_pending_approvals(thread_id, project_path)`**

- List all unanswered requests for a thread

**Tests:** `test_approval_flow.py` (17 tests)

---

## Phase 4: Thread Channels

**File:** `thread_channels.py` (383 lines)

Multi-agent coordination with turn-based protocols.

### Concepts

- **Channel:** Shared coordination space with multiple member threads
- **Turn protocol:** How threads take turns (round_robin or on_demand)
- **Channel state:** Persisted in `.ai/threads/{channel_id}/channel.json`

### Classes

**`ThreadChannelState`**

```python
state = ThreadChannelState(
    channel_id="workflow-123",
    members=[
        {"thread_id": "planner-1739012630", "directive": "plan_feature"},
        {"thread_id": "coder-1739012701", "directive": "implement_plan"},
        {"thread_id": "reviewer-1739012802", "directive": "review_code"},
    ],
    turn_protocol="round_robin",
    turn_order=["planner-...", "coder-...", "reviewer-..."],
    current_turn="planner-1739012630",
    turn_count=0,
)
```

### Functions

**`create_channel(channel_id, members, project_path, turn_protocol="round_robin")`**

- Initialize channel with members
- Support round_robin and on_demand protocols

**`get_channel_state(channel_id, project_path)` / `save_channel_state(state, project_path)`**

- Load/persist channel state

**`advance_turn_round_robin(channel_state)`**

- Move to next member in round-robin
- Increment turn_count

**`can_write_to_channel(origin_thread_id, channel_state)`**

- Check if thread has permission to write
- round_robin: only current_turn holder
- on_demand: any member

**`write_to_channel(channel_id, origin_thread_id, message, project_path, auto_advance=True)`**

- Write message to channel transcript
- Auto-advance turn for round_robin

**`read_channel_transcript(channel_id, project_path, limit=None)`**

- Read messages from channel transcript

### Channel State Structure

```json
{
  "channel_id": "workflow-123",
  "thread_mode": "channel",
  "members": [
    { "thread_id": "planner-1", "directive": "plan" },
    { "thread_id": "coder-1", "directive": "code" }
  ],
  "turn_protocol": "round_robin",
  "turn_order": ["planner-1", "coder-1"],
  "current_turn": "planner-1",
  "turn_count": 5,
  "created_at": "2026-02-10T...",
  "updated_at": "2026-02-10T..."
}
```

**Tests:** `test_thread_channels.py` (20 tests)

---

## Phase 5: TranscriptWatcher

**File:** `transcript_watcher.py` (239 lines)

File-based incremental polling of transcript.jsonl entries.

### Classes

**`TranscriptWatcher`**

- Watches a single thread's transcript
- Tracks file position for incremental reads
- No re-reading of entire file on each poll

```python
watcher = TranscriptWatcher(thread_id, project_path)

# First poll returns all events
events = watcher.poll()  # 10 events

# Append more to transcript
# (e.g., LLM writes more messages)

# Second poll returns only new
new_events = watcher.poll()  # 3 events (only new)
```

**`MultiThreadWatcher`**

- Watch multiple threads simultaneously
- Poll all threads at once

```python
multi = MultiThreadWatcher(project_path)
multi.watch("thread-1")
multi.watch("thread-2")
multi.watch("channel-1")

# Poll all
results = multi.poll_all()
# {
#     "thread-1": [...events...],
#     "thread-2": [...events...],
#     "channel-1": [...events...],
# }
```

### Methods

**`TranscriptWatcher`**

- `poll()` — Get new events since last position
- `reset_position()` — Seek to beginning
- `seek_to_end()` — Skip all existing events (follow mode)
- `get_position()` — Current file position

**`MultiThreadWatcher`**

- `watch(thread_id)` — Register thread for monitoring
- `unwatch(thread_id)` — Stop monitoring
- `poll_all()` — Poll all watched threads
- `get_latest_events(thread_id, count=10)` — Last N events

### Convenience Functions

```python
# Single-use watcher
watcher = watch_thread(thread_id, project_path)

# Get new events (creates temp watcher if needed)
events = get_new_events(thread_id, project_path, watcher=None)
```

**Tests:** `test_transcript_watcher.py` (17 tests)

---

## Phase 6: Integration Tests

**File:** `test_integration.py` (323 lines)

Comprehensive integration tests covering all phases:

- **TestConversationWithApproval:** Conversation + approval gating
- **TestMultiAgentChannelWithWatcher:** Channel coordination + incremental polling
- **TestSaveAndRestoreHarnessState:** State persistence across conversation turns
- **TestConversationReconstruction:** Multi-turn message rebuilding
- **TestApprovalWithTimeout:** Approval timeout scenarios
- **TestChannelRoundRobinWithWatcher:** Turn advancement visibility
- **TestEndToEndThreadWorkflow:** Complete thread lifecycle

**Tests:** `test_integration.py` (8 tests)

---

## Design Principles Implemented

### 1. Filesystem is the Message Bus

- ✅ No IPC/pub-sub, no special queues
- ✅ All state in `.ai/threads/` directory structure
- ✅ Atomic writes via tmp→rename pattern
- ✅ JSON files as data format

### 2. JSONL is Source of Truth

- ✅ `transcript.jsonl` is immutable append-only log
- ✅ Entire conversation reconstructible from transcript
- ✅ Events carry full context (thread_id, timestamp, type, data)

### 3. Data-Driven Configuration

- ✅ Provider YAML defines message reconstruction mappings
- ✅ No Anthropic hardcoding in core functions
- ✅ Message format determined by `provider_config['message_reconstruction']`
- ✅ Different providers can define different schemas

### 4. Turn-Based Coordination

- ✅ Round-robin protocol enforces strict ordering
- ✅ On-demand protocol allows concurrent writes
- ✅ Channel state tracks current turn and turn count
- ✅ Turn advancement automatic or manual

### 5. Cost Tracking Across Turns

- ✅ `SafetyHarness` tracks cumulative cost
- ✅ `harness.cost.to_dict()` serializable for state persistence
- ✅ Turn count and token/spend tracked separately
- ✅ State restored across conversation continuation

---

## File Structure

```
rye/rye/.ai/tools/rye/agent/threads/
├── core_helpers.py                    # Phase 1: Extract helpers
├── conversation_mode.py               # Phase 2: Continue thread
├── approval_flow.py                   # Phase 3: Human approval
├── thread_channels.py                 # Phase 4: Multi-agent channels
├── transcript_watcher.py              # Phase 5: Incremental polling
│
├── test_core_helpers.py               # Phase 1 tests (11)
├── test_conversation_mode.py          # Phase 2 tests (6)
├── test_approval_flow.py              # Phase 3 tests (17)
├── test_thread_channels.py            # Phase 4 tests (20)
├── test_transcript_watcher.py         # Phase 5 tests (17)
├── test_integration.py                # Phase 6 tests (8)
│
├── AGENT_THREADS_IMPLEMENTATION.md    # This file
└── [existing files]
    ├── thread_directive.py
    ├── thread_registry.py
    ├── safety_harness.py
    └── ...
```

---

## Usage Examples

### Example 1: Single-Turn Thread (Existing)

```python
result = await execute(
    directive_name="deployment_planner",
    inputs={"target": "production"},
)
# Thread runs once, status → completed
```

### Example 2: Multi-Turn Conversation

```python
# Initial execution
result1 = await execute(
    directive_name="deployment_planner",
    inputs={"target": "production"},
)
# Thread pauses, awaiting user input

# User provides follow-up
result2 = await continue_thread(
    thread_id="deployment_planner-1739012630",
    message="What's the rollback plan?",
)

# Continue paused → running → paused again
result3 = await continue_thread(
    thread_id="deployment_planner-1739012630",
    message="Proceed with deployment",
)
```

### Example 3: Approval Gate

```python
# LLM suggests deployment
result = await execute(directive_name="deployment_planner")

# Request human approval
request_id = request_approval(
    thread_id=thread_id,
    prompt="Deploy to production?",
    timeout_seconds=600,
)

# Wait for response (blocking or async)
response = await wait_for_approval(request_id, thread_id)

if response["approved"]:
    # Continue with deployment
    await continue_thread(thread_id, "Deploy now")
else:
    # Abort deployment
    print(f"Deployment rejected: {response['message']}")
```

### Example 4: Multi-Agent Channel

```python
# Create channel with agents
create_channel(
    channel_id="feature-delivery",
    members=[
        {"thread_id": "planner-...", "directive": "plan_feature"},
        {"thread_id": "coder-...", "directive": "implement"},
        {"thread_id": "reviewer-...", "directive": "review"},
    ],
    turn_protocol="round_robin",
)

# Agents take turns writing to channel
write_to_channel(
    channel_id="feature-delivery",
    origin_thread_id="planner-...",
    message={"type": "plan", "content": "..."},
)
# Turn auto-advances to next agent

# Monitor with watcher
watcher = TranscriptWatcher("feature-delivery", project_path)
while True:
    events = watcher.poll()
    for event in events:
        print(f"{event['origin_thread']}: {event['type']}")
    time.sleep(1)
```

### Example 5: Watch All Threads

```python
multi = MultiThreadWatcher(project_path)

# Watch multiple threads
for tid in ["thread-1", "thread-2", "channel-1"]:
    multi.watch(tid)

# Poll all periodically
while True:
    results = multi.poll_all()
    for thread_id, events in results.items():
        if events:
            print(f"{thread_id}: {len(events)} new events")
    time.sleep(2)
```

---

## Testing

All 79 tests passing (verified 2026-02-10):

```bash
# Run all agent-threads tests
pytest rye/rye/.ai/tools/rye/agent/threads/test_*.py -v

# Run specific phase
pytest rye/rye/.ai/tools/rye/agent/threads/test_core_helpers.py -v
pytest rye/rye/.ai/tools/rye/agent/threads/test_conversation_mode.py -v
pytest rye/rye/.ai/tools/rye/agent/threads/test_approval_flow.py -v
pytest rye/rye/.ai/tools/rye/agent/threads/test_thread_channels.py -v
pytest rye/rye/.ai/tools/rye/agent/threads/test_transcript_watcher.py -v
pytest rye/rye/.ai/tools/rye/agent/threads/test_integration.py -v
```

---

## Next Steps

### Refactoring Existing Code

The implementation is ready to integrate with existing code in `thread_directive.py`:

1. **Update `thread_directive.py`:**
   - Import `run_llm_loop` from `core_helpers`
   - Replace `_run_tool_use_loop` with call to `run_llm_loop`
   - Add support for `thread_mode` in `thread.json` metadata

2. **Update `execute()` function:**
   - Check for `thread_mode` from directive
   - If `conversation` mode, initialize with `turn_count=0`, `awaiting="user"`

3. **Add conversation endpoints:**
   - New tool or handler for `continue_thread` function

### Future Enhancements

1. **Async Approval Polling:**
   - Replace `wait_for_approval` blocking with async generator
   - Integration with event loop for non-blocking waits

2. **Channel Broadcast:**
   - Messages sent to all members (not turn-based)
   - Useful for announcements/notifications

3. **Transcript Compression:**
   - Archive old JSONL entries to separate files
   - Keep running transcript light-weight

4. **Replay Debugging:**
   - Reconstruct thread state at any point in transcript
   - Useful for debugging agent decisions

---

## References

- **Design Doc:** `docs/rye/design/agent-threads-future.md`
- **Telemetry:** `docs/rye/design/agent-transcript-telemetry.md`
- **SafetyHarness:** `rye/rye/.ai/tools/rye/agent/threads/safety_harness.py`
- **Thread Registry:** `rye/rye/.ai/tools/rye/agent/threads/thread_registry.py`

---

**Implementation complete. All tests passing. Ready for integration with existing thread system.**
