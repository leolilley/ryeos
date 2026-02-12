# Cognition Event Types — Implementation Plan

> Migration from `user_message`/`assistant_text` to `cognition_in`/`cognition_out`

## Overview

Renaming transcript event types to better reflect the AI OS execution model:

- `user_message` → `cognition_in` (context/prompt going into LLM)
- `assistant_text` → `cognition_out` (LLM-generated output)
- `assistant_reasoning` → `cognition_reasoning` (thinking/CoT)

This moves away from the conversational "user/assistant" metaphor toward an execution-centric "cognition flow" model.

## Event Type Mapping

| Old Event Type        | New Event Type        | Notes                     |
| --------------------- | --------------------- | ------------------------- |
| `user_message`        | `cognition_in`        | Input context to LLM      |
| `assistant_text`      | `cognition_out`       | LLM text output           |
| `assistant_reasoning` | `cognition_reasoning` | Thinking/chain-of-thought |

## Files to Update

### 1. Core Implementation Files

#### `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py`

- Line 722: Change `"user_message"` to `"cognition_in"`
- Line 759: Update comment from "assistant_text" to "cognition_out"
- Line 762: Change `"assistant_text"` to `"cognition_out"`
- Line 767: Update warning message
- Line 1135: Update return dict key

#### `rye/rye/.ai/tools/rye/agent/threads/core_helpers.py`

- Lines 115, 179, 289-295: Update event type references
- Lines 328-336: Change `"assistant_text"` to `"cognition_out"`

#### `rye/rye/.ai/tools/rye/agent/threads/transcript_renderer.py`

- Line 135: Change `"user_message"` to `"cognition_in"`
- Line 146: Change `"assistant_text"` to `"cognition_out"`
- Update markdown rendering headers (## User → ## System, ## Assistant → ## Cognition)

#### `rye/rye/.ai/tools/rye/agent/threads/thread_registry.py`

- Line 447: Update docstring
- Line 508: Change `"user_message"` to `"cognition_in"`
- Line 517: Change `"assistant_text"` to `"cognition_out"`

#### `rye/rye/.ai/tools/rye/agent/threads/conversation_mode.py`

- Line 124: Change `"user_message"` to `"cognition_in"`

### 2. Test Files

#### `tests/rye_tests/test_transcript_telemetry.py`

- Lines 81, 85, 90, 100, 105, 110, 111: Update all `assistant_text` references to `cognition_out`
- Lines 104, 110: Update `user_message` references to `cognition_in`
- Lines 137-148: Update test method names and event types
- Lines 214, 234, 252, 368-369, 386, 398: Update remaining references

#### `tests/rye_tests/test_agent_threads_future.py`

- Lines 105, 109-112, 124, 135, 166, 222, 225, 241, 251: Update test cases
- Lines 309, 341, 505, 514, 529, 537: Update test data

### 3. Documentation Files

#### `docs/rye/design/agent-transcript-telemetry.md`

✅ Already updated with new event types

#### `docs/rye/design/agent-threads-future.md`

✅ Already updated with new event types

#### `docs/rye/data-tools/agent/overview.md`

- Line 326: Update event type list

#### `docs/event-architecture-comparison.md`

- Lines 122-124, 133, 140, 143, 413, 421, 548-549: Update references

## Implementation Sequence

### Phase 1: Update Core Implementation (Week 1)

1. Update `thread_directive.py` - main event emission
2. Update `core_helpers.py` - helper functions
3. Update `transcript_renderer.py` - markdown rendering
4. Update `thread_registry.py` - registry operations
5. Update `conversation_mode.py` - conversation handling

### Phase 2: Update Tests (Week 1-2)

1. Update `test_transcript_telemetry.py`
2. Update `test_agent_threads_future.py`
3. Run full test suite to verify changes

### Phase 3: Update Documentation (Week 2)

1. Update remaining doc files
2. Update any code examples in docs
3. Verify consistency across all documentation

### Phase 4: Migration Strategy for Existing Transcripts

**Important**: Existing `.ai/threads/*/transcript.jsonl` files contain old event types.

Options:

1. **Backward Compatibility** (Recommended): Support both old and new event types in reader code

   ```python
   match event.get("type"):
       case "cognition_in" | "user_message":  # Support both
           messages.append({...})
       case "cognition_out" | "assistant_text":  # Support both
           messages.append({...})
   ```

2. **Migration Tool**: Create a one-time migration script

   ```bash
   # Migrate all transcripts to new format
   rye migrate-transcripts --project-path ./
   ```

3. **Clean Slate**: Only apply to new threads (simplest, but loses history)

**Decision**: Implement Option 1 (backward compatibility) in:

- `transcript_renderer.py`
- `thread_registry.py` (read functions)
- Any reconstruction logic

## Markdown Rendering Changes

Old rendering:

```markdown
## System

{system prompt}

---

## Assistant

{assistant response}
```

New rendering:

```markdown
## System

{system prompt}

---

## Cognition

{LLM output}
```

## Configuration Changes

Update `TranscriptOptions` dataclass:

```python
@dataclass
class TranscriptOptions:
    thinking: bool = False
    tool_details: bool = True
    show_cognition_headers: bool = True  # Changed from assistant_metadata
```

## Testing Checklist

- [ ] `thread_directive.py` emits correct event types
- [ ] `transcript_renderer.py` renders new headers correctly
- [ ] Markdown output uses "## Cognition" instead of "## Assistant"
- [ ] Test files use new event types
- [ ] Backward compatibility works for old transcripts
- [ ] Conversation reconstruction handles both old and new types
- [ ] All existing tests pass

## Rollback Plan

If issues arise:

1. Revert code changes in implementation files
2. Revert test changes
3. Regenerate any affected transcripts

The backward compatibility approach minimizes risk since old transcripts will continue to work.

## Notes

- `role` field in `cognition_in` events: Keep as "system" for system prompts, but could extend to other roles in future
- Thread `awaiting` field: Changed from `"user"` to `"input"` to match new terminology
- No changes to JSONL file structure or envelope format
- Event payloads remain the same (only `type` field changes)
