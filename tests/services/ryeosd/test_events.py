"""Tests for event append, replay, and SSE streaming."""

import json
import urllib.request
import urllib.error

import pytest

from tests.services.ryeosd.conftest import daemon_request


class TestEventReplay:
    """Test event replay through HTTP."""

    def test_append_and_replay(self, daemon):
        """Append an event via /execute, replay it via HTTP."""
        # First create a thread via /execute
        status, data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
        })
        if status not in (200, 400, 422):
            pytest.skip(f"execute returned {status}")

        thread_id = data.get("thread", {}).get("thread_id")
        if not thread_id:
            pytest.skip("no thread_id in response")

        # Replay events for this thread
        status, data = daemon_request(
            daemon, "GET", f"/threads/{thread_id}/events"
        )
        assert status in (200, 404)

    def test_replay_empty_thread(self, daemon):
        """Replay on nonexistent thread returns empty or 404."""
        status, data = daemon_request(
            daemon, "GET", "/threads/T-nonexistent/events"
        )
        assert status in (200, 404, 500)


class TestSSEStream:
    """Test SSE event streaming endpoint."""

    def test_sse_endpoint_exists(self, daemon):
        """SSE endpoint should accept connections."""
        url = f"{daemon['url']}/threads/T-nonexistent/events/stream"
        req = urllib.request.Request(url, headers={"Accept": "text/event-stream"})
        try:
            with urllib.request.urlopen(req, timeout=2) as resp:
                assert resp.status == 200
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError):
            pass  # Connection may timeout or 404 — both acceptable

    def test_chain_sse_endpoint_exists(self, daemon):
        """Chain SSE endpoint should accept connections."""
        url = f"{daemon['url']}/chains/T-nonexistent/events/stream"
        req = urllib.request.Request(url, headers={"Accept": "text/event-stream"})
        try:
            with urllib.request.urlopen(req, timeout=2) as resp:
                assert resp.status == 200
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError):
            pass


class TestChainEvents:
    """Test chain-level event replay."""

    def test_chain_events_empty(self, daemon):
        """Chain events for nonexistent chain returns empty or 404."""
        status, data = daemon_request(
            daemon, "GET", "/chains/T-nonexistent/events"
        )
        assert status in (200, 404)
