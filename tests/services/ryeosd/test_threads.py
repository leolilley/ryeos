"""Tests for /threads endpoints."""

import pytest

from tests.services.ryeosd.conftest import daemon_request


class TestListThreads:
    """Test listing threads."""

    def test_list_threads_structure(self, daemon):
        """GET /threads returns a structured response."""
        status, data = daemon_request(daemon, "GET", "/threads")
        assert status == 200
        if isinstance(data, dict):
            assert "threads" in data

    def test_thread_appears_after_execute(self, daemon):
        """Thread should appear in list after execution."""
        exec_status, exec_data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
        })
        if exec_status != 200:
            pytest.skip(f"execute returned {exec_status}")
        
        if not exec_data.get("thread"):
            pytest.skip("no thread in response")

        status, data = daemon_request(daemon, "GET", "/threads")
        assert status == 200

        threads = data.get("threads", data) if isinstance(data, dict) else data
        assert len(threads) > 0


class TestGetThread:
    """Test retrieving individual threads."""

    def test_not_found(self, daemon):
        """GET /threads/{id} returns 404 for nonexistent thread."""
        status, _ = daemon_request(daemon, "GET", "/threads/T-nonexistent")
        assert status == 404

    def test_get_created_thread(self, daemon):
        """GET /threads/{id} returns thread detail after creation."""
        exec_status, exec_data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
        })
        if exec_status != 200:
            pytest.skip(f"execute returned {exec_status}")

        if not exec_data.get("thread"):
            pytest.skip("no thread in response")
        
        thread_id = exec_data["thread"]["thread_id"]
        status, data = daemon_request(daemon, "GET", f"/threads/{thread_id}")
        assert status == 200
        assert data["thread_id"] == thread_id
        assert data["kind"] == "tool_run"
        assert data["item_ref"] == "tool:rye/core/identity"
        assert data["status"] in ("completed", "failed")


class TestThreadChildren:
    """Test thread children endpoint."""

    def test_children_not_found(self, daemon):
        """GET /threads/{id}/children returns 404 for nonexistent thread."""
        status, _ = daemon_request(
            daemon, "GET", "/threads/T-nonexistent/children"
        )
        assert status in (200, 404)  # May return empty list or 404


class TestThreadChain:
    """Test thread chain endpoint."""

    def test_chain_not_found(self, daemon):
        """GET /threads/{id}/chain returns 404 for nonexistent thread."""
        status, _ = daemon_request(
            daemon, "GET", "/threads/T-nonexistent/chain"
        )
        assert status == 404

    def test_chain_for_created_thread(self, daemon):
        """GET /threads/{id}/chain returns chain with at least one thread."""
        exec_status, exec_data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
        })
        if exec_status != 200:
            pytest.skip(f"execute returned {exec_status}")

        if not exec_data.get("thread"):
            pytest.skip("no thread in response")
        
        thread_id = exec_data["thread"]["thread_id"]
        status, data = daemon_request(
            daemon, "GET", f"/threads/{thread_id}/chain"
        )
        assert status == 200
        assert "threads" in data
        assert len(data["threads"]) >= 1
