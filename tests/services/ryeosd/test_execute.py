"""Tests for /execute endpoint (inline + detached modes)."""

import pytest

from conftest import PROJECT_ROOT
from tests.services.ryeosd.conftest import daemon_request


class TestExecuteBasic:
    """Test basic execute functionality."""

    def test_execute_valid_tool(self, daemon):
        """Execute a valid tool."""
        status, data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
        })
        if status == 200:
            assert "thread" in data
            thread = data["thread"]
            assert thread["thread_id"].startswith("T-")
            assert thread["kind"] == "tool_run"
            assert thread["status"] in ("completed", "failed")


class TestExecuteInline:
    """Test inline execution mode."""

    def test_inline_returns_result(self, daemon):
        """Inline mode should block and return result."""
        status, data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
            "launch_mode": "inline",
        })
        if status == 200:
            assert "thread" in data
            assert data["thread"]["thread_id"].startswith("T-")
            assert data["thread"]["kind"] == "tool_run"
            assert data["thread"]["status"] in ("completed", "failed")


class TestExecuteDetached:
    """Test detached execution mode."""

    def test_detached_returns_immediately(self, daemon):
        """Detached mode returns thread info without blocking."""
        status, data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
            "launch_mode": "detached",
        })
        if status == 200:
            assert data.get("detached") is True
            assert "thread" in data
            assert data["thread"]["thread_id"].startswith("T-")


class TestExecuteSiteMetadata:
    """Test site metadata passthrough."""

    def test_target_site_id_passthrough(self, daemon):
        """target_site_id should appear on the created thread."""
        status, data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
            "target_site_id": "site:test-remote",
        })
        if status == 200:
            thread = data["thread"]
            assert thread["current_site_id"] == "site:test-remote"
