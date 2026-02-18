"""HTTP client primitive for making HTTP requests with retry logic."""

import asyncio
import json
import os
import re
import time
from dataclasses import dataclass
from typing import Any, Dict, List, Optional

import httpx


@dataclass
class StreamDestination:
    """Where streaming events go."""
    type: str  # "return" (built-in), or tool-based sinks
    path: Optional[str] = None
    config: Optional[Dict[str, Any]] = None
    format: str = "jsonl"


@dataclass
class StreamConfig:
    """Configuration for streaming HTTP."""
    transport: str  # "sse"
    destinations: List[StreamDestination]
    buffer_events: bool = False
    max_buffer_size: int = 10_000


@dataclass
class HttpResult:
    """Result of HTTP request execution."""
    success: bool
    status_code: int
    body: Any
    headers: Dict[str, str]
    duration_ms: int
    error: Optional[str] = None
    stream_events_count: Optional[int] = None
    stream_destinations: Optional[List[str]] = None


class ReturnSink:
    """Buffer events for inclusion in result."""

    def __init__(self, max_size: int = 10000):
        self.buffer: List[str] = []
        self.max_size = max_size

    async def write(self, event: str) -> None:
        if len(self.buffer) < self.max_size:
            self.buffer.append(event)

    async def close(self) -> None:
        pass

    def get_events(self) -> List[str]:
        return self.buffer


class HttpClientPrimitive:
    """Primitive for making HTTP requests."""

    def __init__(self):
        self._client: Optional[httpx.AsyncClient] = None

    async def execute(self, config: Dict[str, Any], params: Dict[str, Any]) -> HttpResult:
        """Execute an HTTP request based on mode (sync or stream)."""
        mode = params.get("mode", "sync")

        if mode == "sync":
            return await self._execute_sync(config, params)
        elif mode == "stream":
            return await self._execute_stream(config, params)
        else:
            raise ValueError(f"Unknown mode: {mode}. Must be 'sync' or 'stream'")

    async def _execute_sync(self, config: Dict[str, Any], params: Dict[str, Any]) -> HttpResult:
        """Execute a synchronous HTTP request."""
        start_time = time.time()

        try:
            method = config.get("method", "GET").upper()
            url = config.get("url")
            if not url:
                raise ValueError("url is required in config")

            headers = config.get("headers", {})
            body = config.get("body")
            timeout = config.get("timeout", 30)
            retry_config = config.get("retry", {})
            auth_config = config.get("auth", {})

            # Resolve environment variables in URL and headers
            url = self._resolve_env_var(url)
            resolved_headers = {}
            for key, value in headers.items():
                resolved_headers[key] = self._resolve_env_var(str(value))

            # Template URL with params
            url = url.format(**params)

            # Setup authentication
            if auth_config:
                auth_type = auth_config.get("type")
                if auth_type == "bearer":
                    token = self._resolve_env_var(auth_config.get("token", ""))
                    resolved_headers["Authorization"] = f"Bearer {token}"
                elif auth_type == "api_key":
                    key = self._resolve_env_var(auth_config.get("key", ""))
                    key_header = auth_config.get("header", "X-API-Key")
                    resolved_headers[key_header] = key

            client = await self._get_client()

            # Retry logic
            max_attempts = retry_config.get("max_attempts", 1)
            backoff = retry_config.get("backoff", "exponential")
            last_error = None

            for attempt in range(max_attempts):
                try:
                    response = await client.request(
                        method=method,
                        url=url,
                        headers=resolved_headers,
                        content=json.dumps(body) if body and method in ["POST", "PUT", "PATCH"] else None,
                        timeout=timeout,
                    )

                    try:
                        response_body = response.json()
                    except (json.JSONDecodeError, ValueError):
                        response_body = response.text

                    duration_ms = int((time.time() - start_time) * 1000)
                    success = 200 <= response.status_code < 400
                    error_msg = None if success else f"HTTP {response.status_code}: {response.reason_phrase}"

                    return HttpResult(
                        success=success,
                        status_code=response.status_code,
                        body=response_body,
                        headers=dict(response.headers),
                        duration_ms=duration_ms,
                        error=error_msg,
                    )

                except (httpx.TimeoutException, httpx.ConnectError, httpx.RequestError) as e:
                    last_error = str(e)
                    if attempt == max_attempts - 1:
                        break
                    delay = 2**attempt if backoff == "exponential" else 1
                    await asyncio.sleep(delay)

            duration_ms = int((time.time() - start_time) * 1000)
            return HttpResult(
                success=False,
                status_code=0,
                body=None,
                headers={},
                duration_ms=duration_ms,
                error=f"Request failed after {max_attempts} attempts: {last_error}",
            )

        except Exception as e:
            duration_ms = int((time.time() - start_time) * 1000)
            return HttpResult(
                success=False,
                status_code=0,
                body=None,
                headers={},
                duration_ms=duration_ms,
                error=f"Unexpected error: {e}",
            )

    async def _execute_stream(self, config: Dict[str, Any], params: Dict[str, Any]) -> HttpResult:
        """Execute streaming HTTP request (SSE) with destination fan-out."""
        start_time = time.time()

        try:
            sinks = params.pop("__sinks", [])
            should_buffer = any(isinstance(s, ReturnSink) for s in sinks)

            method = config.get("method", "GET").upper()
            url = config.get("url")
            if not url:
                raise ValueError("url is required in config")

            headers = config.get("headers", {})
            body = config.get("body")
            timeout = config.get("timeout", 30)
            auth_config = config.get("auth", {})

            url = self._resolve_env_var(url)
            resolved_headers = {}
            for key, value in headers.items():
                resolved_headers[key] = self._resolve_env_var(str(value))

            url = url.format(**params)

            if auth_config:
                auth_type = auth_config.get("type")
                if auth_type == "bearer":
                    token = self._resolve_env_var(auth_config.get("token", ""))
                    resolved_headers["Authorization"] = f"Bearer {token}"
                elif auth_type == "api_key":
                    key = self._resolve_env_var(auth_config.get("key", ""))
                    key_header = auth_config.get("header", "X-API-Key")
                    resolved_headers[key_header] = key

            client = await self._get_client()

            request_content = None
            if body and method in ["POST", "PUT", "PATCH"]:
                request_content = json.dumps(body)

            async with client.stream(
                method=method,
                url=url,
                headers=resolved_headers,
                content=request_content,
                timeout=timeout,
            ) as response:
                event_count = 0

                async for line in response.aiter_lines():
                    if line.startswith("data:"):
                        event_data = line[5:].strip()
                        if event_data:
                            event_count += 1
                            for sink in sinks:
                                await sink.write(event_data)

                for sink in sinks:
                    await sink.close()

                body = None
                if should_buffer:
                    return_sink = next((s for s in sinks if isinstance(s, ReturnSink)), None)
                    if return_sink:
                        body = return_sink.get_events()

                duration_ms = int((time.time() - start_time) * 1000)
                success = 200 <= response.status_code < 400
                error_msg = None if success else f"HTTP {response.status_code}: {response.reason_phrase}"

                return HttpResult(
                    success=success,
                    status_code=response.status_code,
                    body=body,
                    headers=dict(response.headers),
                    duration_ms=duration_ms,
                    error=error_msg,
                    stream_events_count=event_count,
                    stream_destinations=[type(s).__name__ for s in sinks] if sinks else None,
                )

        except Exception as e:
            sinks = params.get("__sinks", [])
            for sink in sinks:
                try:
                    await sink.close()
                except Exception:
                    pass

            duration_ms = int((time.time() - start_time) * 1000)
            return HttpResult(
                success=False,
                status_code=0,
                body=None,
                headers={},
                duration_ms=duration_ms,
                error=f"Unexpected error: {e}",
            )

    async def _get_client(self) -> httpx.AsyncClient:
        """Get or create HTTP client with connection pooling."""
        if self._client is None:
            self._client = httpx.AsyncClient(
                limits=httpx.Limits(max_keepalive_connections=10, max_connections=20),
                timeout=httpx.Timeout(30.0),
            )
        return self._client

    def _template_body(self, body: Any, params: Dict[str, Any]) -> Any:
        """Recursively template body with parameters, preserving types for single placeholders."""
        if isinstance(body, dict):
            return {k: self._template_body(v, params) for k, v in body.items()}
        elif isinstance(body, list):
            return [self._template_body(item, params) for item in body]
        elif isinstance(body, str):
            match = re.match(r'^\{(\w+)\}$', body.strip())
            if match:
                param_name = match.group(1)
                if param_name in params:
                    return params[param_name]
                else:
                    raise ValueError(f"Missing parameter for template: {param_name}")
            else:
                try:
                    return body.format(**params)
                except KeyError as e:
                    raise ValueError(f"Missing parameter for template: {e}")
        else:
            return body

    def _resolve_env_var(self, value: str) -> str:
        """Resolve environment variables with syntax: ${VAR:-default}"""
        if not isinstance(value, str):
            return str(value)

        pattern = r"\$\{([^}]+)\}"

        def replace_var(match):
            var_expr = match.group(1)
            if ":-" in var_expr:
                var_name, default_value = var_expr.split(":-", 1)
                return os.environ.get(var_name.strip(), default_value)
            else:
                return os.environ.get(var_expr, "")

        return re.sub(pattern, replace_var, value)

    async def close(self):
        """Close the HTTP client."""
        if self._client:
            await self._client.aclose()
            self._client = None
