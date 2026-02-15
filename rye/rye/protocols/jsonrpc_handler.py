"""
JSON-RPC 2.0 Protocol Handler

Provides utilities for building, parsing, and validating JSON-RPC 2.0 messages.
"""

import json
import uuid
from dataclasses import dataclass
from typing import Any, Dict, Optional, Union


@dataclass
class JsonRpcRequest:
    """JSON-RPC 2.0 request structure."""

    method: str
    params: Dict[str, Any]
    id: Optional[Union[str, int]] = None

    def __post_init__(self):
        """Generate ID if not provided."""
        if self.id is None:
            self.id = str(uuid.uuid4())

    def to_dict(self) -> Dict[str, Any]:
        """Convert to JSON-RPC 2.0 request dict."""
        return {
            "jsonrpc": "2.0",
            "method": self.method,
            "params": self.params,
            "id": self.id,
        }

    def to_json(self) -> str:
        """Convert to JSON string."""
        return json.dumps(self.to_dict())

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "JsonRpcRequest":
        """Create from dict (for testing/debugging)."""
        return cls(
            method=data["method"],
            params=data.get("params", {}),
            id=data.get("id"),
        )


@dataclass
class JsonRpcResponse:
    """JSON-RPC 2.0 response structure."""

    result: Optional[Any] = None
    error: Optional[Dict[str, Any]] = None
    id: Optional[Union[str, int]] = None

    @property
    def is_error(self) -> bool:
        """Check if response is an error."""
        return self.error is not None

    @property
    def is_success(self) -> bool:
        """Check if response is successful."""
        return self.error is None and self.result is not None

    def to_dict(self) -> Dict[str, Any]:
        """Convert to JSON-RPC 2.0 response dict."""
        response = {"jsonrpc": "2.0", "id": self.id}
        if self.error:
            response["error"] = self.error
        else:
            response["result"] = self.result
        return response

    def to_json(self) -> str:
        """Convert to JSON string."""
        return json.dumps(self.to_dict())

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "JsonRpcResponse":
        """Parse JSON-RPC 2.0 response from dict."""
        if "error" in data:
            return cls(
                error=data["error"],
                id=data.get("id"),
            )
        else:
            return cls(
                result=data.get("result"),
                id=data.get("id"),
            )

    @classmethod
    def from_json(cls, json_str: str) -> "JsonRpcResponse":
        """Parse JSON-RPC 2.0 response from JSON string."""
        data = json.loads(json_str)
        return cls.from_dict(data)

    @classmethod
    def success(
        cls, result: Any, request_id: Optional[Union[str, int]] = None
    ) -> "JsonRpcResponse":
        """Create success response."""
        return cls(result=result, id=request_id)

    @classmethod
    def error_response(
        cls,
        code: int,
        message: str,
        data: Optional[Any] = None,
        request_id: Optional[Union[str, int]] = None,
    ) -> "JsonRpcResponse":
        """Create error response."""
        error = {"code": code, "message": message}
        if data is not None:
            error["data"] = data
        return cls(error=error, id=request_id)


class JsonRpcBuilder:
    """Builder for JSON-RPC requests with template support."""

    @staticmethod
    def build_request(
        method: str,
        params: Dict[str, Any],
        request_id: Optional[Union[str, int]] = None,
    ) -> JsonRpcRequest:
        """Build a JSON-RPC request."""
        return JsonRpcRequest(method=method, params=params, id=request_id)

    @staticmethod
    def build_from_template(
        template: Dict[str, Any],
        params: Dict[str, Any],
    ) -> JsonRpcRequest:
        """Build request from template with parameter substitution.

        Template can have placeholders like {param_name} that get replaced
        with values from params dict.
        """
        # Extract method from template
        method = template.get("method", "")
        if isinstance(method, str) and method.startswith("{") and method.endswith("}"):
            param_name = method[1:-1]
            method = params.get(param_name, method)

        # Extract and template params
        template_params = template.get("params", {})
        templated_params = JsonRpcBuilder._template_dict(template_params, params)

        # Extract request ID if provided
        request_id = template.get("id")
        if (
            isinstance(request_id, str)
            and request_id.startswith("{")
            and request_id.endswith("}")
        ):
            param_name = request_id[1:-1]
            request_id = params.get(param_name, request_id)

        return JsonRpcRequest(method=method, params=templated_params, id=request_id)

    @staticmethod
    def _template_dict(template: Any, params: Dict[str, Any]) -> Any:
        """Recursively template a dict/list/str with params.

        Similar to http_client's _template_body but for JSON-RPC params.
        """
        if isinstance(template, dict):
            return {
                k: JsonRpcBuilder._template_dict(v, params) for k, v in template.items()
            }
        elif isinstance(template, list):
            return [JsonRpcBuilder._template_dict(item, params) for item in template]
        elif isinstance(template, str):
            # Check if entire string is a single placeholder
            import re

            match = re.match(r"^{(\w+)}$", template.strip())
            if match:
                param_name = match.group(1)
                if param_name in params:
                    return params[param_name]  # Preserve type
                else:
                    raise ValueError(f"Missing parameter for template: {param_name}")
            else:
                # Use format() for strings with mixed content
                try:
                    return template.format(**params)
                except KeyError as e:
                    raise ValueError(f"Missing parameter for template: {e}")
        else:
            return template


class JsonRpcParser:
    """Parser for JSON-RPC responses."""

    @staticmethod
    def parse_response(json_str: str) -> JsonRpcResponse:
        """Parse JSON-RPC response from JSON string."""
        try:
            data = json.loads(json_str)
            return JsonRpcResponse.from_dict(data)
        except json.JSONDecodeError as e:
            # Return parse error response
            return JsonRpcResponse.error_response(
                code=-32700,
                message="Parse error",
                data=str(e),
            )

    @staticmethod
    def parse_batch_responses(json_str: str) -> list[JsonRpcResponse]:
        """Parse batch JSON-RPC responses (array of responses)."""
        try:
            data = json.loads(json_str)
            if isinstance(data, list):
                return [JsonRpcResponse.from_dict(item) for item in data]
            else:
                # Single response
                return [JsonRpcResponse.from_dict(data)]
        except json.JSONDecodeError as e:
            return [
                JsonRpcResponse.error_response(
                    code=-32700,
                    message="Parse error",
                    data=str(e),
                )
            ]

    @staticmethod
    def validate_response(response: JsonRpcResponse) -> tuple[bool, Optional[str]]:
        """Validate JSON-RPC response structure.

        Returns:
            (is_valid, error_message)
        """
        if response.id is None:
            return False, "Response missing 'id' field"

        if response.error is not None:
            if "code" not in response.error or "message" not in response.error:
                return False, "Error response missing 'code' or 'message'"

        if response.result is None and response.error is None:
            return False, "Response must have either 'result' or 'error'"

        return True, None


# Standard JSON-RPC error codes
class JsonRpcErrorCodes:
    """Standard JSON-RPC 2.0 error codes."""

    PARSE_ERROR = -32700
    INVALID_REQUEST = -32600
    METHOD_NOT_FOUND = -32601
    INVALID_PARAMS = -32602
    INTERNAL_ERROR = -32603

    # Server error range: -32000 to -32099
    SERVER_ERROR_START = -32000
    SERVER_ERROR_END = -32099
