# rye:signed:2026-02-16T05:55:29Z:fa48640a150887f2badd94469933b6a1da057d47d426549d542a814d3bf13cca:eC-N_yPQJ0vI4rxO_mGT01kzIC6zamHCaNf3Hze1cVLUiKDjvR4tEgTQbpaeBNWRVhG--z7pQ5OvfFEa0hCSDA==:440443d0858f0199
"""
http_provider.py: ProviderAdapter that dispatches through the tool execution chain.

Delegates HTTP calls to the provider tool (e.g., rye/agent/providers/anthropic)
via ToolDispatcher → ExecuteTool → PrimitiveExecutor → http_client primitive.
The primitive handles auth, env resolution, retries, HTTP transport.

This adapter only handles:
1. Formatting messages/tools into params the provider tool expects
2. Parsing the API response using the provider YAML's tool_use.response config
"""

__version__ = "1.1.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/adapters"
__tool_description__ = "HTTP provider adapter for LLM API calls"

import logging
import os
from typing import Any, Dict, List, Optional

from .provider_adapter import ProviderAdapter

logger = logging.getLogger(__name__)


class HttpProvider(ProviderAdapter):
    """Provider that dispatches LLM calls through the tool execution chain.

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
        self._prompts = provider_config.get("prompts", {})

    def _convert_messages(self, messages: List[Dict]) -> List[Dict]:
        """Convert runner message format to provider format.

        The runner produces tool results as:
            {"role": "tool", "tool_call_id": "...", "content": "..."}

        The provider YAML's tool_use.tool_result config defines the target format.
        For Anthropic, this becomes:
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "...", "content": "..."}]}
        """
        result_config = self._tool_use.get("tool_result", {})
        role = result_config.get("role", "user")
        block_type = result_config.get("block_type", "tool_result")
        id_field = result_config.get("id_field", "tool_use_id")
        content_field = result_config.get("content_field", "content")
        error_field = result_config.get("error_field", "is_error")

        resp_config = self._tool_use.get("response", {})
        tool_use_block_type = resp_config.get("tool_use_block_type", "tool_use")
        tool_use_id_field = resp_config.get("tool_use_id_field", "id")
        tool_use_name_field = resp_config.get("tool_use_name_field", "name")
        tool_use_input_field = resp_config.get("tool_use_input_field", "input")
        text_block_type = resp_config.get("text_block_type", "text")
        text_field = resp_config.get("text_field", "text")

        converted = []
        pending_tool_results = []

        for msg in messages:
            if msg.get("role") == "tool":
                block = {
                    "type": block_type,
                    id_field: msg.get("tool_call_id", ""),
                    content_field: msg.get("content", ""),
                }
                if msg.get("is_error"):
                    block[error_field] = True
                pending_tool_results.append(block)
            elif msg.get("role") == "assistant" and msg.get("tool_calls"):
                if pending_tool_results:
                    converted.append({"role": role, "content": pending_tool_results})
                    pending_tool_results = []
                # Reconstruct assistant content blocks
                content_blocks = []
                text = msg.get("content", "")
                if text:
                    content_blocks.append({"type": text_block_type, text_field: text})
                for tc in msg["tool_calls"]:
                    content_blocks.append({
                        "type": tool_use_block_type,
                        tool_use_id_field: tc["id"],
                        tool_use_name_field: tc["name"],
                        tool_use_input_field: tc["input"],
                    })
                converted.append({"role": "assistant", "content": content_blocks})
            else:
                if pending_tool_results:
                    converted.append({"role": role, "content": pending_tool_results})
                    pending_tool_results = []
                converted.append(msg)

        if pending_tool_results:
            converted.append({"role": role, "content": pending_tool_results})

        return converted

    def _format_tools(self, tools: List[Dict]) -> List[Dict]:
        """Format tool schemas using tool_use.tool_definition from provider config.

        The YAML defines field mapping via template placeholders:
            tool_definition:
              name: "{name}"
              description: "{description}"
              input_schema: "{schema}"

        Generic tool schemas use: name, description, schema.
        The config maps these to whatever the provider API expects.
        """
        if not tools:
            return tools
        tool_def_template = self._tool_use.get("tool_definition", {})
        if not tool_def_template:
            return tools

        import re
        formatted = []
        for tool in tools:
            entry = {}
            for key, value in tool_def_template.items():
                if isinstance(value, str):
                    match = re.match(r"^\{(\w+)\}$", value.strip())
                    if match:
                        param = match.group(1)
                        entry[key] = tool.get(param, "")
                    else:
                        entry[key] = value
                else:
                    entry[key] = value
            formatted.append(entry)
        return formatted

    def _parse_response(self, response_body: Dict) -> Dict:
        """Parse API response using tool_use.response config."""
        resp_config = self._tool_use.get("response", {})

        stop_reason_field = resp_config.get("stop_reason_field", "stop_reason")
        content_field = resp_config.get("content_field", "content")
        text_block_type = resp_config.get("text_block_type", "text")
        text_field = resp_config.get("text_field", "text")
        tool_use_block_type = resp_config.get("tool_use_block_type", "tool_use")
        tool_use_id_field = resp_config.get("tool_use_id_field", "id")
        tool_use_name_field = resp_config.get("tool_use_name_field", "name")
        tool_use_input_field = resp_config.get("tool_use_input_field", "input")

        content_blocks = response_body.get(content_field, [])
        finish_reason = response_body.get(stop_reason_field, "end_turn")

        text_parts = []
        tool_calls = []

        for block in content_blocks:
            block_type = block.get("type", "")
            if block_type == text_block_type:
                text_parts.append(block.get(text_field, ""))
            elif block_type == tool_use_block_type:
                tool_calls.append({
                    "id": block.get(tool_use_id_field, ""),
                    "name": block.get(tool_use_name_field, ""),
                    "input": block.get(tool_use_input_field, {}),
                })

        usage = response_body.get("usage", {})
        input_tokens = usage.get("input_tokens", 0)
        output_tokens = usage.get("output_tokens", 0)

        pricing = self.config.get("pricing", {}).get(self.model, {})
        input_cost = pricing.get("input", 0.0)
        output_cost = pricing.get("output", 0.0)
        spend = (input_tokens * input_cost + output_tokens * output_cost) / 1_000_000

        return {
            "text": "\n".join(text_parts),
            "tool_calls": tool_calls,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "spend": spend,
            "finish_reason": finish_reason,
        }

    async def create_completion(self, messages: List[Dict], tools: List[Dict]) -> Dict:
        """Send messages to LLM via the tool execution chain."""
        converted_messages = self._convert_messages(messages)
        formatted_tools = self._format_tools(tools) if tools else []

        params = {
            "model": self.model,
            "messages": converted_messages,
            "max_tokens": 4096,
        }
        if formatted_tools:
            params["tools"] = formatted_tools

        result = await self._dispatcher.dispatch({
            "primary": "execute",
            "item_type": "tool",
            "item_id": self._provider_item_id,
            "params": params,
        })

        if result.get("status") != "success":
            from ..errors import ProviderCallError

            # Debug: log full dispatch result
            if os.environ.get("RYE_DEBUG"):
                logger.error("Provider dispatch failed: %s", result)

            data = result.get("data", {})

            # Priority 1: Tool-chain error (lockfile, permission, resolution)
            chain_error = result.get("error") or data.get("error")
            if chain_error and not isinstance(data.get("body"), dict):
                raise ProviderCallError(
                    provider_id=self._provider_item_id,
                    message=str(chain_error),
                    error_type="tool_chain_error",
                )

            # Priority 2: HTTP API error with structured body
            body = data.get("body", {})
            http_status = data.get("status_code")
            headers = data.get("headers", {})
            request_id = headers.get("request-id", "")

            if isinstance(body, dict) and "error" in body:
                api_error = body["error"]
                if isinstance(api_error, dict):
                    error_msg = api_error.get("message", str(api_error))
                    error_type = api_error.get("type", "api_error")
                else:
                    error_msg = str(api_error)
                    error_type = "api_error"
            else:
                error_msg = chain_error or str(body or "Unknown provider error")
                error_type = "unknown"

            raise ProviderCallError(
                provider_id=self._provider_item_id,
                message=error_msg,
                http_status=http_status,
                request_id=request_id,
                error_type=error_type,
                retryable=http_status in (429, 500, 502, 503, 529) if http_status else False,
            )

        data = result.get("data", {})
        response_body = data.get("body", data)
        return self._parse_response(response_body)
