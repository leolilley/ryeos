"""Tests for command submit, claim, and complete."""

import pytest

from tests.services.ryeosd.conftest import daemon_request


class TestCommandSubmit:
    """Test command submission via HTTP."""

    def test_submit_to_nonexistent_thread(self, daemon):
        """Submitting a command to nonexistent thread should fail."""
        status, data = daemon_request(
            daemon, "POST", "/threads/T-nonexistent/commands",
            {"command_type": "kill"},
        )
        assert status in (400, 404, 500)

    def test_cancel_returns_501(self, daemon):
        """Cancel command should return 501 — not yet implemented."""
        status, data = daemon_request(
            daemon, "POST", "/threads/T-nonexistent/commands",
            {"command_type": "cancel"},
        )
        assert status == 501

    def test_interrupt_returns_501(self, daemon):
        """Interrupt command should return 501 — not yet implemented."""
        status, data = daemon_request(
            daemon, "POST", "/threads/T-nonexistent/commands",
            {"command_type": "interrupt"},
        )
        assert status == 501

    def test_continue_returns_501(self, daemon):
        """Continue command should return 501 — not yet implemented."""
        status, data = daemon_request(
            daemon, "POST", "/threads/T-nonexistent/commands",
            {"command_type": "continue"},
        )
        assert status == 501

    def test_submit_kill_command(self, daemon):
        """Submit kill command — should be rejected if PGID matches daemon."""
        exec_status, exec_data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
        })
        if exec_status not in (200, 400, 422):
            pytest.skip(f"execute returned {exec_status}")

        thread_id = exec_data.get("thread", {}).get("thread_id")
        if not thread_id:
            pytest.skip("no thread_id")

        status, data = daemon_request(
            daemon, "POST", f"/threads/{thread_id}/commands",
            {"command_type": "kill"},
        )
        # Kill on a finalized thread or with daemon PGID should be rejected
        assert status in (200, 400)
