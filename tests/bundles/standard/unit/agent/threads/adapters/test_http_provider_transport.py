"""Tests for http_provider.py httpx transport layer (post-HTTP-primitive removal)."""

import os

import pytest

from conftest import PROJECT_ROOT
from module_loader import load_module

_THREADS_ANCHOR = (
    PROJECT_ROOT
    / "ryeos" / "bundles" / "standard" / "ryeos_std" / ".ai" / "tools" / "rye" / "agent" / "threads"
)

_mod = load_module("adapters/http_provider", anchor=_THREADS_ANCHOR)

_HttpResult = _mod._HttpResult
_ReturnSink = _mod._ReturnSink
_resolve_env = _mod._resolve_env


# ── _HttpResult dataclass ────────────────────────────────────────────────


class TestHttpResult:
    def test_http_result_success(self):
        r = _HttpResult(success=True, status_code=200, body={"ok": True})
        assert r.success is True
        assert r.status_code == 200
        assert r.body == {"ok": True}

    def test_http_result_failure(self):
        r = _HttpResult(success=False, status_code=500, error="server error")
        assert r.success is False
        assert r.status_code == 500
        assert r.error == "server error"

    def test_http_result_defaults(self):
        r = _HttpResult(success=True, status_code=200)
        assert r.body is None
        assert r.headers == {}
        assert r.error is None


# ── _ReturnSink ──────────────────────────────────────────────────────────


class TestReturnSink:
    async def test_return_sink_buffers_events(self):
        sink = _ReturnSink()
        await sink.write("event1")
        await sink.write("event2")
        await sink.write("event3")
        assert sink.get_events() == ["event1", "event2", "event3"]

    async def test_return_sink_empty(self):
        sink = _ReturnSink()
        assert sink.get_events() == []

    async def test_return_sink_write_is_async(self):
        """write() is awaitable (coroutine)."""
        import asyncio
        sink = _ReturnSink()
        coro = sink.write("x")
        assert asyncio.iscoroutine(coro)
        await coro
        assert sink.get_events() == ["x"]


# ── _resolve_env ─────────────────────────────────────────────────────────


class TestResolveEnv:
    def test_resolve_env_simple(self):
        result = _resolve_env("${HOME}")
        assert result == os.environ["HOME"]

    def test_resolve_env_default(self):
        result = _resolve_env("${_RYE_TEST_NONEXISTENT_VAR_:-fallback}")
        assert result == "fallback"

    def test_resolve_env_no_match(self):
        result = _resolve_env("plain string with no vars")
        assert result == "plain string with no vars"
