"""Tests for HTTP client primitive."""

import pytest
from lillux.primitives.http_client import HttpResult, HttpClientPrimitive, ReturnSink


class TestHttpResult:
    """Test HttpResult dataclass."""

    def test_create_success_result(self):
        """Create successful HttpResult."""
        result = HttpResult(
            success=True,
            status_code=200,
            body={"data": "value"},
            headers={"content-type": "application/json"},
            duration_ms=100,
        )
        assert result.success is True
        assert result.status_code == 200
        assert result.body == {"data": "value"}
        assert result.error is None

    def test_create_failure_result(self):
        """Create failed HttpResult."""
        result = HttpResult(
            success=False,
            status_code=500,
            body=None,
            headers={},
            duration_ms=50,
            error="Server error",
        )
        assert result.success is False
        assert result.status_code == 500
        assert result.error == "Server error"

    def test_with_streaming_metadata(self):
        """HttpResult supports streaming metadata."""
        result = HttpResult(
            success=True,
            status_code=200,
            body=[],
            headers={},
            duration_ms=200,
            stream_events_count=5,
            stream_destinations=["ReturnSink"],
        )
        assert result.stream_events_count == 5
        assert result.stream_destinations == ["ReturnSink"]


class TestReturnSink:
    """Test ReturnSink built-in sink."""

    @pytest.mark.asyncio
    async def test_sink_buffers_events(self):
        """ReturnSink buffers events."""
        sink = ReturnSink()
        await sink.write("event1")
        await sink.write("event2")
        
        events = sink.get_events()
        assert len(events) == 2
        assert events[0] == "event1"

    @pytest.mark.asyncio
    async def test_sink_max_size(self):
        """ReturnSink respects max_size."""
        sink = ReturnSink(max_size=2)
        await sink.write("event1")
        await sink.write("event2")
        await sink.write("event3")  # Should be dropped
        
        events = sink.get_events()
        assert len(events) == 2

    @pytest.mark.asyncio
    async def test_sink_close(self):
        """ReturnSink close is a no-op."""
        sink = ReturnSink()
        await sink.write("event")
        await sink.close()
        
        events = sink.get_events()
        assert len(events) == 1


class TestHttpClientPrimitive:
    """Test HttpClientPrimitive basic functionality."""

    @pytest.mark.asyncio
    async def test_execute_requires_url(self):
        """execute() returns error when URL missing."""
        primitive = HttpClientPrimitive()
        
        result = await primitive.execute(config={}, params={})
        
        assert result.success is False
        assert "url is required" in result.error

    @pytest.mark.asyncio
    async def test_execute_returns_result(self):
        """execute() returns HttpResult on network error."""
        primitive = HttpClientPrimitive()
        result = await primitive.execute(
            config={"url": "https://invalid-domain-9999999.test"},
            params={}
        )
        
        assert isinstance(result, HttpResult)
        assert result.success is False
        assert result.duration_ms >= 0

    @pytest.mark.asyncio
    async def test_mode_defaults_to_sync(self):
        """Default mode is sync."""
        primitive = HttpClientPrimitive()
        result = await primitive.execute(
            config={"url": "https://invalid.test"},
            params={}
        )
        
        # Should use sync mode by default
        assert isinstance(result, HttpResult)

    @pytest.mark.asyncio
    async def test_mode_stream_requires_sinks(self):
        """Stream mode works with sinks."""
        primitive = HttpClientPrimitive()
        sink = ReturnSink()
        
        result = await primitive.execute(
            config={"url": "https://invalid.test"},
            params={"mode": "stream", "__sinks": [sink]}
        )
        
        assert isinstance(result, HttpResult)

    def test_template_body_preserves_types(self):
        """Body templating preserves types for single placeholders."""
        primitive = HttpClientPrimitive()
        
        body = {
            "model": "{model}",
            "messages": "{messages}",
            "temperature": "{temperature}",
        }
        params = {
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": 0.7,
        }
        
        result = primitive._template_body(body, params)
        
        assert result["model"] == "gpt-4"
        assert isinstance(result["messages"], list)
        assert isinstance(result["temperature"], float)

    def test_template_body_string_interpolation(self):
        """Body templating does string interpolation for mixed content."""
        primitive = HttpClientPrimitive()
        
        body = {"query": "Search for {term} in {category}"}
        params = {"term": "python", "category": "books"}
        
        result = primitive._template_body(body, params)
        
        assert result["query"] == "Search for python in books"

    def test_resolve_env_var_with_default(self):
        """Env var resolution with default syntax."""
        primitive = HttpClientPrimitive()
        
        result = primitive._resolve_env_var("${NONEXISTENT_VAR:-default_value}")
        
        assert result == "default_value"

    def test_resolve_env_var_without_default(self):
        """Env var resolution without default returns empty string."""
        primitive = HttpClientPrimitive()
        
        result = primitive._resolve_env_var("${NONEXISTENT_VAR}")
        
        assert result == ""

    @pytest.mark.asyncio
    async def test_close(self):
        """Close cleans up client."""
        primitive = HttpClientPrimitive()
        
        # Access client to create it
        await primitive._get_client()
        assert primitive._client is not None
        
        await primitive.close()
        assert primitive._client is None
