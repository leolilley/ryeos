# Data-Driven Provider Spec — Generalized Response Parsing & Message Conversion

> **Status:** Implementation plan — upgrades http_provider.py to be fully data-driven via provider YAML schemas. Eliminates hardcoded format handlers. Adding a new LLM provider becomes a YAML-only operation.

---

## The Problem

`http_provider.py` currently dispatches to two hardcoded parser methods based on `response_format`:

```python
# Line 56-62 — named format dispatch
def _response_format(self) -> str:
    return self._tool_use.get("response_format", "content_blocks")

# Line 213-217 — hardcoded if/else
def _parse_response(self, response_body):
    if self._response_format == "chat_completion":
        return self._parse_response_chat(response_body)
    return self._parse_response_blocks(response_body)
```

`_parse_response_blocks()` (Anthropic) already reads field names from the YAML config — `text_block_type`, `tool_use_block_type`, `tool_use_id_field`, etc. It's almost data-driven. But the response structure navigation (where the content array lives, how blocks are detected, where usage fields live) is hardcoded per format.

Adding a new provider (e.g., Gemini via Zen) currently means adding a third named parser method. This doesn't scale and contradicts the data-driven philosophy.

---

## The Insight

All LLM APIs return the same four things: **text**, **tool calls**, **usage**, and **finish reason**. They just nest them differently. The structural variation reduces to two content modes:

| Mode       | Pattern                                                                          | Providers                                                  |
| ---------- | -------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| `blocks`   | Content is an array of mixed items — text and tool calls interleaved in one list | Anthropic, Gemini, any future block-based API              |
| `separate` | Text is a direct string field, tool_calls is a separate array                    | OpenAI, OpenAI-compatible (MiniMax, GLM, Kimi, Qwen, etc.) |

Within each mode, the differences are just field names and nesting depth — solvable with dot-path navigation.

---

## The Solution: `response_schema` in Provider YAML

### Dot-Path Navigation

A utility function that navigates nested structures via dot-separated paths:

```python
def _resolve_path(self, obj: Any, path: str) -> Any:
    """Navigate 'candidates.0.content.parts' through nested dicts/lists."""
    for key in path.split("."):
        if obj is None:
            return None
        if isinstance(obj, list):
            try:
                obj = obj[int(key)]
            except (IndexError, ValueError):
                return None
        elif isinstance(obj, dict):
            obj = obj.get(key)
        else:
            return None
    return obj
```

This already exists conceptually in `_apply_template()` which does recursive placeholder substitution. `_resolve_path()` is the read-side equivalent.

### Block Detection

Two detection modes, configurable per block type:

```yaml
# By field value (Anthropic pattern)
text: { field: "type", value: "text" }

# By key presence (Gemini pattern)
text: { key: "text" }
```

```python
def _detect_block(self, block: dict, detect_config: dict) -> bool:
    if "field" in detect_config:
        return block.get(detect_config["field"]) == detect_config["value"]
    if "key" in detect_config:
        return detect_config["key"] in block
    return False
```

---

## Provider YAML Schemas

### Anthropic (`content_blocks` → `blocks` mode)

Current YAML `tool_use.response` section gets replaced with `response_schema`:

```yaml
response_schema:
  content_mode: blocks
  content_path: "content"
  block_detect:
    text: { field: "type", value: "text" }
    tool_call: { field: "type", value: "tool_use" }
  text_value: "text"
  tool_call_name: "name"
  tool_call_input: "input"
  tool_call_id: "id"
  usage_path: "usage"
  input_tokens: "input_tokens"
  output_tokens: "output_tokens"
  finish_reason_path: "stop_reason"
  finish_reason_tool_use: "tool_use"
```

### OpenAI (`chat_completion` → `separate` mode)

```yaml
response_schema:
  content_mode: separate
  content_path: "choices.0.message"
  text_field: "content"
  tool_calls_field: "tool_calls"
  tool_call_name: "function.name"
  tool_call_input: "function.arguments"
  tool_call_input_format: json_string
  tool_call_id: "id"
  usage_path: "usage"
  input_tokens: "prompt_tokens"
  output_tokens: "completion_tokens"
  finish_reason_path: "choices.0.finish_reason"
  finish_reason_tool_use: "tool_calls"
```

### Gemini via Zen (`blocks` mode, different paths)

```yaml
response_schema:
  content_mode: blocks
  content_path: "candidates.0.content.parts"
  block_detect:
    text: { key: "text" }
    tool_call: { key: "functionCall" }
  text_value: "text"
  tool_call_name: "functionCall.name"
  tool_call_input: "functionCall.args"
  tool_call_id: null # auto-generate UUID
  usage_path: "usageMetadata"
  input_tokens: "promptTokenCount"
  output_tokens: "candidatesTokenCount"
  finish_reason_path: "candidates.0.finishReason"
  finish_reason_tool_use: "TOOL_CALLS"
```

No code changes needed for Gemini — just this YAML. The generic parser handles it via `blocks` mode + dot-path navigation.

---

## Generic Response Parser

Replaces `_parse_response_blocks()` and `_parse_response_chat()` with one method:

```python
def _parse_response(self, response_body: Dict) -> Dict:
    """Parse any LLM API response using response_schema from provider YAML."""
    import json
    import uuid

    schema = self._tool_use.get("response_schema", {})
    mode = schema.get("content_mode", "blocks")

    text_parts = []
    tool_calls = []

    if mode == "blocks":
        content_path = schema.get("content_path", "content")
        blocks = self._resolve_path(response_body, content_path) or []
        detect = schema.get("block_detect", {})

        for block in blocks:
            if self._detect_block(block, detect.get("text", {})):
                text_parts.append(
                    self._resolve_path(block, schema.get("text_value", "text")) or ""
                )
            elif self._detect_block(block, detect.get("tool_call", {})):
                name = self._resolve_path(block, schema["tool_call_name"]) or ""
                raw_input = self._resolve_path(block, schema["tool_call_input"]) or {}
                tc_id_path = schema.get("tool_call_id")
                tc_id = (
                    self._resolve_path(block, tc_id_path)
                    if tc_id_path
                    else str(uuid.uuid4())
                )
                tool_calls.append({"id": tc_id, "name": name, "input": raw_input})

    elif mode == "separate":
        message = self._resolve_path(response_body, schema.get("content_path", "")) or {}
        text_parts.append(message.get(schema.get("text_field", "content")) or "")

        raw_calls = message.get(schema.get("tool_calls_field", "tool_calls")) or []
        input_format = schema.get("tool_call_input_format")

        for tc in raw_calls:
            name = self._resolve_path(tc, schema["tool_call_name"]) or ""
            raw_input = self._resolve_path(tc, schema["tool_call_input"]) or {}
            if input_format == "json_string" and isinstance(raw_input, str):
                try:
                    raw_input = json.loads(raw_input)
                except (json.JSONDecodeError, ValueError):
                    raw_input = {"_raw": raw_input}
            tc_id = self._resolve_path(tc, schema.get("tool_call_id", "id")) or ""
            tool_calls.append({"id": tc_id, "name": name, "input": raw_input})

    # Usage — always via dot-path
    usage_obj = self._resolve_path(response_body, schema.get("usage_path", "usage")) or {}
    input_tokens = usage_obj.get(schema.get("input_tokens", "input_tokens"), 0)
    output_tokens = usage_obj.get(schema.get("output_tokens", "output_tokens"), 0)

    # Finish reason — via dot-path
    finish_reason = (
        self._resolve_path(response_body, schema.get("finish_reason_path", "stop_reason"))
        or "stop"
    )

    # Cost
    pricing = self.config.get("pricing", {}).get(self.model, {})
    spend = (
        input_tokens * pricing.get("input", 0.0)
        + output_tokens * pricing.get("output", 0.0)
    ) / 1_000_000

    return {
        "text": "\n".join(text_parts) if len(text_parts) > 1 else (text_parts[0] if text_parts else ""),
        "tool_calls": tool_calls,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "spend": spend,
        "finish_reason": finish_reason,
    }
```

~60 lines. Replaces ~100 lines of two hardcoded parsers. No provider names anywhere.

---

## Message Conversion: `message_schema`

Same approach — replace `_convert_messages_blocks()` and `_convert_messages_chat()` with one generic converter driven by `message_schema` in the YAML.

### The Schema

```yaml
message_schema:
  # Request body structure
  messages_key: "contents" # "messages" for OpenAI/Anthropic

  # Role mapping (internal → provider)
  role_map:
    user: "user"
    assistant: "model" # Gemini uses "model", others use "assistant"

  # Content wrapping
  content_key: "parts" # "content" for OpenAI/Anthropic
  content_wrap: "parts_array" # how to wrap text content

  # System prompt handling
  system_prompt:
    mode: "top_level" # "top_level" | "role"
    key: "systemInstruction" # request body field (top_level mode)
    wrap: "parts" # {parts: [{text: "..."}]}

  # Tool result formatting
  tool_result:
    role: "function" # "user" for Anthropic, "tool" for OpenAI
    wrap_mode: "parts" # how results are wrapped
    block_template: # uses same _apply_template() system
      functionResponse:
        name: "{tool_name}"
        response:
          content: "{content}"

  # Assistant tool call reconstruction
  tool_call_block_template:
    functionCall:
      name: "{name}"
      args: "{input}"
```

### Content Wrap Modes

| Mode           | Produces                              | Used By   |
| -------------- | ------------------------------------- | --------- |
| `string`       | `"hello"`                             | OpenAI    |
| `blocks_array` | `[{"type": "text", "text": "hello"}]` | Anthropic |
| `parts_array`  | `[{"text": "hello"}]`                 | Gemini    |

### System Prompt Modes

| Mode            | Behavior                                                         | Used By  |
| --------------- | ---------------------------------------------------------------- | -------- |
| `role`          | Injected as `{"role": "system", "content": "..."}` first message | OpenAI   |
| `top_level`     | Set as `body[key]` (e.g., `body.systemInstruction`)              | Gemini   |
| `first_message` | Prepended to first user message content                          | Fallback |

### Per-Provider YAML Examples

**Anthropic:**

```yaml
message_schema:
  messages_key: "messages"
  role_map: { user: "user", assistant: "assistant" }
  content_key: "content"
  content_wrap: "blocks_array"
  system_prompt:
    mode: "role"
  tool_result:
    role: "user"
    wrap_mode: "content_blocks"
    block_template:
      type: "tool_result"
      tool_use_id: "{tool_call_id}"
      content: "{content}"
  tool_call_block_template:
    type: "tool_use"
    id: "{id}"
    name: "{name}"
    input: "{input}"
```

**OpenAI:**

```yaml
message_schema:
  messages_key: "messages"
  role_map: { user: "user", assistant: "assistant" }
  content_key: "content"
  content_wrap: "string"
  system_prompt:
    mode: "role"
  tool_result:
    role: "tool"
    wrap_mode: "direct"
    block_template:
      tool_call_id: "{tool_call_id}"
      content: "{content}"
  tool_call_block_template:
    type: "function"
    id: "{id}"
    function:
      name: "{name}"
      arguments: "{input_json}"
```

**Gemini (Zen):**

```yaml
message_schema:
  messages_key: "contents"
  role_map: { user: "user", assistant: "model" }
  content_key: "parts"
  content_wrap: "parts_array"
  system_prompt:
    mode: "top_level"
    key: "systemInstruction"
    wrap: "parts"
  tool_result:
    role: "function"
    wrap_mode: "parts"
    block_template:
      functionResponse:
        name: "{tool_name}"
        response:
          content: "{content}"
  tool_call_block_template:
    functionCall:
      name: "{name}"
      args: "{input}"
```

---

## Stream Assembly

Streaming SSE events also differ structurally. Same `blocks` vs `separate` distinction applies, plus the event envelope format:

| Provider  | Event Envelope                                    | Content Deltas                                   |
| --------- | ------------------------------------------------- | ------------------------------------------------ |
| Anthropic | `{"type": "content_block_delta", "delta": {...}}` | `text_delta`, `input_json_delta` per block       |
| OpenAI    | `{"choices": [{"delta": {...}}]}`                 | `content` string delta, `tool_calls` array delta |
| Gemini    | `{"candidates": [{"content": {"parts": [...]}}]}` | Complete parts per chunk                         |

### Stream Schema

```yaml
stream_schema:
  # How events arrive
  event_envelope: "candidates.0" # path to the main event data
  done_signal: null # "[DONE]" for OpenAI, null for others

  # Text deltas
  text_delta_path: "content.parts.*.text" # where text chunks appear

  # Tool call deltas
  tool_delta_path: "content.parts.*.functionCall"
  tool_delta_name: "name"
  tool_delta_args: "args"

  # Usage (often in final event)
  usage_path: "usageMetadata"
  input_tokens: "promptTokenCount"
  output_tokens: "candidatesTokenCount"

  # Finish
  finish_reason_path: "finishReason"
```

The generic stream assembler iterates events, extracting text and tool call deltas via the schema paths. Same `_resolve_path()` utility.

---

## Implementation Plan

One pass. Replace the old code, update the provider YAMLs, done.

### Steps

1. Add `_resolve_path()` and `_detect_block()` utilities to `http_provider.py`
2. Replace `_parse_response_blocks()` and `_parse_response_chat()` with single `_parse_response()` that reads `response_schema`
3. Replace `_convert_messages_blocks()` and `_convert_messages_chat()` with single `_convert_messages()` that reads `message_schema`
4. Replace `_assemble_anthropic_stream()` and `_assemble_openai_stream()` with single `_assemble_stream_response()` that reads `stream_schema`
5. Remove `response_format` property entirely
6. Update `anthropic.yaml` and `openai.yaml` — replace `tool_use.response` with `response_schema`, add `message_schema` and `stream_schema`
7. Create `zen.yaml` with Gemini schemas
8. Sign all three provider YAMLs + `http_provider.py`

---

## What This Enables

- **Adding Zen/Gemini:** YAML file only. Zero code changes.
- **Adding any OpenAI-compatible provider:** Copy `openai.yaml`, change URL and auth. Zero code changes.
- **Adding a provider with a new API format:** If it's `blocks` or `separate` mode — YAML only. If it's genuinely new structure — add a third `content_mode` value and ~20 lines of handler.
- **Testing:** Each provider's YAML schema can be tested against recorded API responses independently — just `_parse_response(recorded_response)` and assert the output shape.

---

## Effort

| What                                            | Lines                                       |
| ----------------------------------------------- | ------------------------------------------- |
| Generic parser (`_parse_response`)              | ~60 (replaces ~100)                         |
| Generic message converter (`_convert_messages`) | ~80 (replaces ~110)                         |
| Generic stream assembler                        | ~70 (replaces ~140)                         |
| `_resolve_path` + `_detect_block` utilities     | ~20                                         |
| Provider YAML updates (anthropic + openai)      | ~60 each                                    |
| New zen.yaml                                    | ~100                                        |
| Removed legacy code                             | ~350                                        |
| **Net**                                         | **~-90 lines** (less code, more capability) |
