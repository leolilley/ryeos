# rye:signed:2026-02-26T06:42:42Z:3855cf2af62565da879e5d0783c99d0555b88b1cc113781d3e132c4594f031f4:hjTtCjklEpLiQ6EV1J4buC7ZkPFLp3vOXkcIgjM-ne1RIb0wsulDmj8-KMeJIXf-LLkStMYJpkG7aJ9P7fUzDQ==:4b987fd4e40303ac
"""
http_provider.py: ProviderAdapter that dispatches through the tool execution chain.

Delegates HTTP calls to the provider tool (e.g., rye/agent/providers/anthropic)
via ToolDispatcher → ExecuteTool → PrimitiveExecutor → http_client primitive.
The primitive handles auth, env resolution, retries, HTTP transport.

This adapter only handles:
1. Formatting messages/tools into params the provider tool expects
2. Parsing the API response using the provider YAML's response_schema config
3. Converting messages using the provider YAML's message_schema config
4. Assembling streaming events using the provider YAML's stream_schema config

All provider-specific behavior is driven by YAML schemas — no hardcoded format handlers.
"""

__version__ = "2.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/adapters"
__tool_description__ = "HTTP provider adapter for LLM API calls"

import json
import logging
import os
import uuid
from typing import Any, Dict, List, Optional

from .provider_adapter import ProviderAdapter

logger = logging.getLogger(__name__)


class HttpProvider(ProviderAdapter):
    """Provider that dispatches LLM calls through the tool execution chain.

    Fully data-driven: response parsing, message conversion, and stream assembly
    are all configured via provider YAML schemas (response_schema, message_schema,
    stream_schema). No provider-specific code paths.

    Args:
        model: Resolved model ID (e.g., "claude-3-5-haiku-20241022")
        provider_config: Full provider YAML config dict
        dispatcher: ToolDispatcher instance for dispatching tool calls
        provider_item_id: Tool item_id for the provider (e.g., "rye/agent/providers/anthropic")
    """

    def __init__(
        self,
        model: str,
        provider_config: Dict,
        dispatcher: Any,
        provider_item_id: str,
    ):
        super().__init__(model, provider_config)
        self._dispatcher = dispatcher
        self._provider_item_id = provider_item_id
        self._tool_use = provider_config.get("tool_use", {})
        self._http_config = provider_config.get("config", {})

    @property
    def supports_streaming(self) -> bool:
        return True

    @property
    def _response_format(self) -> str:
        stream_schema = self._tool_use.get("stream_schema", {})
        return stream_schema.get("stream_mode", "content_blocks")

    # ── Utilities ──────────────────────────────────────────────────────

    def _resolve_path(self, obj: Any, path: str) -> Any:
        """Navigate nested dicts/lists via dot-separated path.

        Example: _resolve_path(data, "choices.0.message") navigates
        data["choices"][0]["message"].
        """
        if not path:
            return obj
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

    def _detect_block(self, block: dict, detect_config: dict) -> bool:
        """Check if a content block matches a detection rule.

        Supports two modes:
        - field/value: block["type"] == "text"
        - key presence: "text" in block
        """
        if not detect_config:
            return False
        if "field" in detect_config:
            return block.get(detect_config["field"]) == detect_config["value"]
        if "key" in detect_config:
            return detect_config["key"] in block
        return False

    def _wrap_text_block(self, text: str, mode: str) -> Any:
        """Wrap text content per content_wrap mode."""
        if mode == "blocks_array":
            return {"type": "text", "text": text}
        elif mode == "parts_array":
            return {"text": text}
        return text

    # ── Message Conversion ─────────────────────────────────────────────

    def _convert_messages(self, messages: List[Dict], system_prompt: str = "") -> List[Dict]:
        """Convert runner message format to provider format using message_schema.

        Handles three concerns driven by YAML config:
        1. Tool result messages → provider-specific format (grouped or individual)
        2. Assistant messages with tool_calls → reconstructed with provider block format
        3. Regular messages → role-mapped and content-wrapped if needed
        4. System prompt → prepended as system role message (when system_mode is "message")
        """
        schema = self._tool_use.get("message_schema", {})
        role_map = schema.get("role_map", {"user": "user", "assistant": "assistant"})
        content_key = schema.get("content_key", "content")
        content_wrap = schema.get("content_wrap", "string")
        tr_config = schema.get("tool_result", {})
        tc_template = schema.get("tool_call_block_template", {})

        tr_role = tr_config.get("role", "user")
        tr_wrap = tr_config.get("wrap_mode", "content_blocks")
        tr_template = tr_config.get("block_template", {})
        tr_error_field = tr_config.get("error_field")

        converted = []
        pending_results = []
        # Map tool_call_id → name for providers that need name in tool results (Gemini)
        tc_name_map: Dict[str, str] = {}

        def flush_results():
            nonlocal pending_results
            if pending_results:
                converted.append({"role": tr_role, content_key: pending_results})
                pending_results = []

        for msg in messages:
            role = msg.get("role", "")

            if role == "tool":
                tc_id = msg.get("tool_call_id", "")
                tool_name = msg.get("tool_name", msg.get("name", ""))
                if not tool_name:
                    tool_name = tc_name_map.get(tc_id, "")
                data = {
                    "tool_call_id": tc_id,
                    "tool_name": tool_name,
                    "content": msg.get("content", ""),
                }
                block = self._apply_template(tr_template, data)
                if msg.get("is_error") and tr_error_field:
                    block[tr_error_field] = True

                if tr_wrap == "content_blocks":
                    pending_results.append(block)
                elif tr_wrap == "direct":
                    result_msg = {"role": tr_role}
                    result_msg.update(block)
                    converted.append(result_msg)
                elif tr_wrap == "parts":
                    converted.append({"role": tr_role, content_key: [block]})

            elif role == "assistant" and msg.get("tool_calls"):
                flush_results()
                assistant_role = role_map.get("assistant", "assistant")
                for tc in msg["tool_calls"]:
                    tc_name_map[tc["id"]] = tc["name"]

                if content_wrap == "string":
                    # OpenAI: tool_calls as top-level array on the message
                    assistant_msg = {
                        "role": assistant_role,
                        "content": msg.get("content") or None,
                    }
                    tc_list = []
                    for tc in msg["tool_calls"]:
                        tc_data = {
                            "id": tc["id"],
                            "name": tc["name"],
                            "input": tc["input"],
                            "input_json": json.dumps(tc["input"])
                            if isinstance(tc["input"], dict)
                            else str(tc["input"]),
                        }
                        tc_list.append(self._apply_template(tc_template, tc_data))
                    assistant_msg["tool_calls"] = tc_list
                    converted.append(assistant_msg)
                else:
                    # Block-based: tool calls are content blocks (Anthropic, Gemini)
                    blocks = []
                    thinking = msg.get("_thinking", "")
                    if thinking:
                        blocks.append({"thought": True, "text": thinking})
                    text = msg.get("content", "")
                    if text:
                        blocks.append(self._wrap_text_block(text, content_wrap))
                    for tc in msg["tool_calls"]:
                        if "_raw_block" in tc:
                            # Replay raw block (preserves thoughtSignature for Gemini)
                            blocks.append(tc["_raw_block"])
                        else:
                            tc_data = {
                                "id": tc["id"],
                                "name": tc["name"],
                                "input": tc["input"],
                            }
                            blocks.append(self._apply_template(tc_template, tc_data))
                    converted.append({"role": assistant_role, content_key: blocks})

            else:
                flush_results()
                mapped_role = role_map.get(role, role)
                if content_key == "content":
                    # Pass through as-is (Anthropic/OpenAI accept string content)
                    out = dict(msg)
                    if mapped_role != role:
                        out["role"] = mapped_role
                    converted.append(out)
                else:
                    # Different content key (e.g., Gemini "parts")
                    parts = []
                    thinking = msg.get("_thinking", "")
                    if thinking:
                        parts.append({"thought": True, "text": thinking})
                    content = msg.get("content", "")
                    if content:
                        parts.append(self._wrap_text_block(content, content_wrap))
                    converted.append({"role": mapped_role, content_key: parts})

        flush_results()

        # Prepend system message for providers that use message-role system prompts
        sys_config = self._tool_use.get("system_message", {})
        if system_prompt and sys_config.get("mode") == "message_role":
            converted.insert(0, {"role": "system", "content": system_prompt})

        return converted

    # ── Tool Formatting ────────────────────────────────────────────────

    def _format_tools(self, tools: List[Dict]) -> List[Dict]:
        """Format tool schemas using tool_use.tool_definition from provider config.

        The YAML defines field mapping via template placeholders:
            Anthropic: {name: "{name}", description: "{description}", input_schema: "{schema}"}
            OpenAI:    {type: function, function: {name: "{name}", parameters: "{schema}"}}
            Gemini:    {name: "{name}", ...} + tool_list_wrap: "functionDeclarations"

        When tool_list_wrap is set, all formatted tools are grouped into a single
        object under that key (e.g., Gemini needs [{functionDeclarations: [...all...]}]).
        """
        if not tools:
            return tools
        tool_def_template = self._tool_use.get("tool_definition", {})
        if not tool_def_template:
            return tools
        formatted = [self._apply_template(tool_def_template, tool) for tool in tools]
        wrap_key = self._tool_use.get("tool_list_wrap")
        if wrap_key:
            return [{wrap_key: formatted}]
        return formatted

    def _apply_template(self, template: Any, tool: Dict) -> Any:
        """Recursively apply template placeholders from data dict."""
        import re
        if isinstance(template, str):
            match = re.match(r"^\{(\w+)\}$", template.strip())
            if match:
                return tool.get(match.group(1), "")
            return template
        if isinstance(template, dict):
            return {k: self._apply_template(v, tool) for k, v in template.items()}
        if isinstance(template, list):
            return [self._apply_template(item, tool) for item in template]
        return template

    # ── Response Parsing ───────────────────────────────────────────────

    def _parse_response(self, response_body: Dict) -> Dict:
        """Parse any LLM API response using response_schema from provider YAML."""
        schema = self._tool_use.get("response_schema", {})
        mode = schema.get("content_mode", "blocks")

        text_parts = []
        thinking_parts = []
        tool_calls = []

        if mode == "blocks":
            content_path = schema.get("content_path", "content")
            blocks = self._resolve_path(response_body, content_path) or []
            detect = schema.get("block_detect", {})

            for block in blocks:
                if self._detect_block(block, detect.get("thinking", {})):
                    thinking_parts.append(
                        self._resolve_path(block, schema.get("text_value", "text")) or ""
                    )
                elif self._detect_block(block, detect.get("text", {})):
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
                    tc = {"id": tc_id, "name": name, "input": raw_input}
                    # Preserve raw block for providers that need it (Gemini thoughtSignature)
                    if "thoughtSignature" in block:
                        tc["_raw_block"] = block
                    tool_calls.append(tc)

        elif mode == "separate":
            message = (
                self._resolve_path(response_body, schema.get("content_path", ""))
                or {}
            )
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
                tc_id = (
                    self._resolve_path(tc, schema.get("tool_call_id", "id")) or ""
                )
                tool_calls.append({"id": tc_id, "name": name, "input": raw_input})

        # Usage — always via dot-path
        usage_obj = (
            self._resolve_path(response_body, schema.get("usage_path", "usage")) or {}
        )
        input_tokens = usage_obj.get(schema.get("input_tokens", "input_tokens"), 0)
        output_tokens = usage_obj.get(schema.get("output_tokens", "output_tokens"), 0)

        # Finish reason — via dot-path
        finish_reason = (
            self._resolve_path(
                response_body, schema.get("finish_reason_path", "stop_reason")
            )
            or "stop"
        )

        # Cost
        pricing = self.config.get("pricing", {}).get(self.model, {})
        spend = (
            input_tokens * pricing.get("input", 0.0)
            + output_tokens * pricing.get("output", 0.0)
        ) / 1_000_000

        result = {
            "text": "\n".join(text_parts)
            if len(text_parts) > 1
            else (text_parts[0] if text_parts else ""),
            "tool_calls": tool_calls,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "spend": spend,
            "finish_reason": finish_reason,
        }
        if thinking_parts:
            result["thinking"] = "\n".join(thinking_parts)
        return result

    # ── HTTP Execution ────────────────────────────────────────────────

    def _build_body(self, params: Dict) -> Dict:
        """Template the body config with params, substituting {key} placeholders."""
        body_template = self._http_config.get("body", {})
        return self._apply_template(body_template, params)

    async def _get_http_client(self):
        """Lazily import and return the http_client primitive."""
        from lillux.primitives.http_client import HttpClientPrimitive
        if not hasattr(self, "_http_client"):
            self._http_client = HttpClientPrimitive()
        return self._http_client

    def _inject_system_prompt(self, body: Dict, system: str) -> None:
        """Inject system prompt into the request body using profile config.

        Uses the system_message config from the profile's tool_use section:
        - mode "body_field": sets body[field] = system (Anthropic)
        - mode "body_inject": deep-merges a template structure (Gemini)
        - mode "message_role": handled in _convert_messages, not here
        """
        sys_config = self._tool_use.get("system_message", {})
        mode = sys_config.get("mode", "body_field")

        if mode == "body_field":
            field = sys_config.get("field", "system")
            body[field] = system
        elif mode == "body_inject":
            template = sys_config.get("template", {})
            if template:
                body.update(self._apply_template(template, {"system": system}))

    async def _execute_http(self, params: Dict) -> Dict:
        """Execute HTTP request directly using merged provider config.

        Uses the http_client primitive directly with the merged config
        (including profile overrides). This avoids re-loading the raw YAML
        through the dispatch chain, which wouldn't have profile merges.
        """
        config = dict(self._http_config)
        mode = params.pop("mode", "sync")
        system = params.pop("system", "")

        # Use stream_url when streaming if the provider defines one (e.g., Gemini)
        url_key = "stream_url" if mode == "stream" and "stream_url" in config else "url"
        config["url"] = config.get(url_key, config.get("url", "")).format(**params)
        config["body"] = self._build_body(params)

        # Inject system prompt into request body if provided
        if system:
            self._inject_system_prompt(config["body"], system)

        client = await self._get_http_client()

        if mode == "stream":
            return await client._execute_stream(config, params)
        return await client._execute_sync(config, params)

    def _raise_on_error(self, result, streaming: bool = False):
        """Convert http_client result to ProviderCallError if failed."""
        from ..errors import ProviderCallError

        if result.success:
            return

        if os.environ.get("RYE_DEBUG"):
            logger.error("Provider HTTP failed: status=%s body=%s error=%s", result.status_code, result.body, result.error)

        body = result.body if isinstance(result.body, dict) else {}
        http_status = result.status_code
        request_id = result.headers.get("request-id", "") if result.headers else ""

        if isinstance(body, dict) and "error" in body:
            api_error = body["error"]
            if isinstance(api_error, dict):
                error_msg = api_error.get("message", str(api_error))
                error_type = api_error.get("type", "api_error")
            else:
                error_msg = str(api_error)
                error_type = "api_error"
        else:
            error_msg = result.error or str(body or "Unknown provider error")
            error_type = "unknown"

        raise ProviderCallError(
            provider_id=self._provider_item_id,
            message=error_msg,
            http_status=http_status,
            request_id=request_id,
            error_type=error_type,
            retryable=http_status in (0, 429, 500, 502, 503, 529) if http_status is not None else True,
        )

    # ── Completion ─────────────────────────────────────────────────────

    async def create_completion(
        self, messages: List[Dict], tools: List[Dict], system_prompt: str = ""
    ) -> Dict:
        """Send messages to LLM via direct HTTP call using merged provider config."""
        converted_messages = self._convert_messages(messages, system_prompt=system_prompt)
        formatted_tools = self._format_tools(tools) if tools else []

        params = {
            "model": self.model,
            "messages": converted_messages,
            "max_tokens": 16384,
        }
        if formatted_tools:
            params["tools"] = formatted_tools
        if system_prompt:
            params["system"] = system_prompt

        result = await self._execute_http(params)
        self._raise_on_error(result)

        response_body = result.body if isinstance(result.body, dict) else {}
        return self._parse_response(response_body)

    async def create_streaming_completion(
        self, messages: List[Dict], tools: List[Dict], sinks: Optional[List] = None,
        system_prompt: str = "",
    ) -> Dict:
        """Send messages to LLM via streaming, with real-time sink fan-out.

        Sinks receive raw SSE events as they arrive (for transcript writing).
        A ReturnSink is always added to buffer events for final response assembly.

        Returns the same response dict as create_completion().
        """
        from lillux.primitives.http_client import ReturnSink

        converted_messages = self._convert_messages(messages, system_prompt=system_prompt)
        formatted_tools = self._format_tools(tools) if tools else []

        params = {
            "model": self.model,
            "messages": converted_messages,
            "max_tokens": 16384,
            "stream": True,
            "mode": "stream",
        }
        if formatted_tools:
            params["tools"] = formatted_tools
        if system_prompt:
            params["system"] = system_prompt

        return_sink = ReturnSink()
        all_sinks = [return_sink] + (sinks or [])
        params["__sinks"] = all_sinks

        result = await self._execute_http(params)
        self._raise_on_error(result, streaming=True)

        events = return_sink.get_events()
        return self._assemble_stream_response(events)

    # ── Stream Assembly ────────────────────────────────────────────────

    def _assemble_stream_response(self, events: List[str]) -> Dict:
        """Assemble buffered SSE events into response dict using stream_schema."""
        schema = self._tool_use.get("stream_schema", {})
        mode = schema.get("stream_mode", "event_typed")

        if mode == "delta_merge":
            return self._assemble_delta_merge(events, schema)
        if mode == "complete_chunks":
            return self._assemble_complete_chunks(events, schema)
        return self._assemble_event_typed(events, schema)

    def _assemble_event_typed(self, events: List[str], schema: Dict) -> Dict:
        """Assemble event-typed SSE stream (Anthropic pattern).

        Events have a type field that determines their structure:
        message_start, content_block_start, content_block_delta, message_delta.
        Field names and paths are all driven by stream_schema + response_schema.
        """
        event_type_field = schema.get("event_type_field", "type")
        block_start_path = schema.get("block_start_path", "content_block")

        resp_schema = self._tool_use.get("response_schema", {})
        block_detect = resp_schema.get("block_detect", {})

        text_parts = []
        tool_calls = []
        finish_reason = "end_turn"
        input_tokens = 0
        output_tokens = 0

        for raw in events:
            try:
                data = json.loads(raw)
            except (json.JSONDecodeError, ValueError):
                continue

            event_type = data.get(event_type_field, "")

            if event_type == schema.get("message_start_type", "message_start"):
                usage = (
                    self._resolve_path(
                        data, schema.get("message_start_usage", "message.usage")
                    )
                    or {}
                )
                input_tokens += usage.get(
                    resp_schema.get("input_tokens", "input_tokens"), 0
                )
                output_tokens += usage.get(
                    resp_schema.get("output_tokens", "output_tokens"), 0
                )

            elif event_type == schema.get("block_start_type", "content_block_start"):
                block = self._resolve_path(data, block_start_path) or {}
                if self._detect_block(block, block_detect.get("tool_call", {})):
                    tc_id_path = resp_schema.get("tool_call_id")
                    tc_id = (
                        self._resolve_path(block, tc_id_path)
                        if tc_id_path
                        else str(uuid.uuid4())
                    )
                    tool_calls.append({
                        "id": tc_id,
                        "name": self._resolve_path(
                            block, resp_schema.get("tool_call_name", "name")
                        ) or "",
                        "input_parts": [],
                    })

            elif event_type == schema.get("block_delta_type", "content_block_delta"):
                delta = self._resolve_path(
                    data, schema.get("delta_path", "delta")
                ) or {}
                delta_type = delta.get(
                    schema.get("delta_type_field", "type"), ""
                )
                if delta_type == schema.get("text_delta_type", "text_delta"):
                    text_parts.append(
                        delta.get(schema.get("text_delta_field", "text"), "")
                    )
                elif delta_type == schema.get(
                    "tool_input_delta_type", "input_json_delta"
                ):
                    if tool_calls:
                        tool_calls[-1]["input_parts"].append(
                            delta.get(
                                schema.get("tool_input_delta_field", "partial_json"),
                                "",
                            )
                        )

            elif event_type == schema.get("message_delta_type", "message_delta"):
                fr = self._resolve_path(
                    data, schema.get("finish_reason_path", "delta.stop_reason")
                )
                if fr:
                    finish_reason = fr
                usage = (
                    self._resolve_path(
                        data, schema.get("delta_usage_path", "usage")
                    )
                    or {}
                )
                output_tokens += usage.get(
                    resp_schema.get("output_tokens", "output_tokens"), 0
                )

        assembled_calls = []
        for tc in tool_calls:
            input_str = "".join(tc["input_parts"])
            try:
                inp = json.loads(input_str) if input_str else {}
            except (json.JSONDecodeError, ValueError):
                inp = {"_raw": input_str}
            assembled_calls.append({"id": tc["id"], "name": tc["name"], "input": inp})

        pricing = self.config.get("pricing", {}).get(self.model, {})
        spend = (
            input_tokens * pricing.get("input", 0.0)
            + output_tokens * pricing.get("output", 0.0)
        ) / 1_000_000

        return {
            "text": "".join(text_parts),
            "tool_calls": assembled_calls,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "spend": spend,
            "finish_reason": finish_reason,
        }

    def _assemble_delta_merge(self, events: List[str], schema: Dict) -> Dict:
        """Assemble delta-merge SSE stream (OpenAI pattern).

        Events are progressive deltas with choices array. Text and tool call
        fragments are merged across events. Field names driven by stream_schema.
        """
        done_signal = schema.get("done_signal", "[DONE]")
        resp_schema = self._tool_use.get("response_schema", {})

        text_parts = []
        tool_calls = {}  # index -> {id, name, arguments_parts}
        finish_reason = "stop"
        input_tokens = 0
        output_tokens = 0

        in_tok_field = schema.get(
            "input_tokens_field", resp_schema.get("input_tokens", "prompt_tokens")
        )
        out_tok_field = schema.get(
            "output_tokens_field", resp_schema.get("output_tokens", "completion_tokens")
        )

        for raw in events:
            if raw == done_signal:
                continue
            try:
                data = json.loads(raw)
            except (json.JSONDecodeError, ValueError):
                continue

            choices = data.get(schema.get("choices_field", "choices"), [])
            if not choices:
                usage = (
                    self._resolve_path(data, schema.get("usage_path", "usage"))
                    or {}
                )
                input_tokens += usage.get(in_tok_field, 0)
                output_tokens += usage.get(out_tok_field, 0)
                continue

            choice = choices[0]
            delta = choice.get(schema.get("delta_field", "delta"), {})

            text_field = schema.get("text_delta_field", "content")
            if text_field in delta and delta[text_field]:
                text_parts.append(delta[text_field])

            tc_field = schema.get("tool_calls_field", "tool_calls")
            for tc in delta.get(tc_field, []):
                idx = tc.get(schema.get("tool_call_index_field", "index"), 0)
                if idx not in tool_calls:
                    tool_calls[idx] = {"id": "", "name": "", "arguments_parts": []}
                tc_id_field = schema.get("tool_call_id_field", "id")
                if tc.get(tc_id_field):
                    tool_calls[idx]["id"] = tc[tc_id_field]
                func = (
                    self._resolve_path(
                        tc, schema.get("tool_call_func_path", "function")
                    )
                    or {}
                )
                name_field = schema.get("tool_call_name_field", "name")
                args_field = schema.get("tool_call_args_field", "arguments")
                if func.get(name_field):
                    tool_calls[idx]["name"] = func[name_field]
                if func.get(args_field):
                    tool_calls[idx]["arguments_parts"].append(func[args_field])

            fr_field = schema.get("finish_reason_field", "finish_reason")
            if choice.get(fr_field):
                finish_reason = choice[fr_field]

            usage = (
                self._resolve_path(data, schema.get("usage_path", "usage")) or {}
            )
            input_tokens += usage.get(in_tok_field, 0)
            output_tokens += usage.get(out_tok_field, 0)

        assembled_calls = []
        for idx in sorted(tool_calls):
            tc = tool_calls[idx]
            args_str = "".join(tc["arguments_parts"])
            try:
                args = json.loads(args_str) if args_str else {}
            except (json.JSONDecodeError, ValueError):
                args = {"_raw": args_str}
            assembled_calls.append({"id": tc["id"], "name": tc["name"], "input": args})

        pricing = self.config.get("pricing", {}).get(self.model, {})
        spend = (
            input_tokens * pricing.get("input", 0.0)
            + output_tokens * pricing.get("output", 0.0)
        ) / 1_000_000

        return {
            "text": "".join(text_parts),
            "tool_calls": assembled_calls,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "spend": spend,
            "finish_reason": finish_reason,
        }

    def _assemble_complete_chunks(self, events: List[str], schema: Dict) -> Dict:
        """Assemble complete-chunk SSE stream (Gemini pattern).

        Each event is a complete response-like object with candidates/parts.
        Reuses response_schema to extract content from each chunk, then
        accumulates text and tool calls across chunks.
        """
        resp_schema = self._tool_use.get("response_schema", {})
        done_signal = schema.get("done_signal")
        detect = resp_schema.get("block_detect", {})
        content_path = resp_schema.get("content_path", "content")

        text_parts = []
        thinking_parts = []
        tool_calls = []
        input_tokens = 0
        output_tokens = 0
        finish_reason = "stop"

        for raw in events:
            if done_signal and raw == done_signal:
                continue
            try:
                data = json.loads(raw)
            except (json.JSONDecodeError, ValueError):
                continue

            blocks = self._resolve_path(data, content_path) or []

            for block in blocks:
                if self._detect_block(block, detect.get("thinking", {})):
                    thinking_parts.append(
                        self._resolve_path(
                            block, resp_schema.get("text_value", "text")
                        ) or ""
                    )
                elif self._detect_block(block, detect.get("text", {})):
                    text_parts.append(
                        self._resolve_path(
                            block, resp_schema.get("text_value", "text")
                        ) or ""
                    )
                elif self._detect_block(block, detect.get("tool_call", {})):
                    name = (
                        self._resolve_path(block, resp_schema["tool_call_name"])
                        or ""
                    )
                    raw_input = (
                        self._resolve_path(block, resp_schema["tool_call_input"])
                        or {}
                    )
                    tc_id_path = resp_schema.get("tool_call_id")
                    tc_id = (
                        self._resolve_path(block, tc_id_path)
                        if tc_id_path
                        else str(uuid.uuid4())
                    )
                    tc = {"id": tc_id, "name": name, "input": raw_input}
                    if "thoughtSignature" in block:
                        tc["_raw_block"] = block
                    tool_calls.append(tc)

            # Usage — Gemini reports cumulative, take the max
            usage_obj = (
                self._resolve_path(data, resp_schema.get("usage_path", "usage"))
                or {}
            )
            chunk_in = usage_obj.get(
                resp_schema.get("input_tokens", "input_tokens"), 0
            )
            chunk_out = usage_obj.get(
                resp_schema.get("output_tokens", "output_tokens"), 0
            )
            input_tokens = max(input_tokens, chunk_in)
            output_tokens = max(output_tokens, chunk_out)

            fr = self._resolve_path(
                data, resp_schema.get("finish_reason_path", "stop_reason")
            )
            if fr:
                finish_reason = fr

        pricing = self.config.get("pricing", {}).get(self.model, {})
        spend = (
            input_tokens * pricing.get("input", 0.0)
            + output_tokens * pricing.get("output", 0.0)
        ) / 1_000_000

        result = {
            "text": "".join(text_parts),
            "tool_calls": tool_calls,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "spend": spend,
            "finish_reason": finish_reason,
        }
        if thinking_parts:
            result["thinking"] = "".join(thinking_parts)
        return result
