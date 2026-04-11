"""Tests for kill safety — daemon must not kill its own process group."""

import os

import pytest

from tests.services.ryeosd.conftest import daemon_request


class TestKillSafety:
    """Verify the daemon survives kill commands."""

    def test_daemon_survives_kill_on_completed_thread(self, daemon):
        """Kill on a completed thread should not crash the daemon."""
        exec_status, exec_data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
        })
        if exec_status != 200:
            pytest.skip(f"execute returned {exec_status}")

        if not exec_data.get("thread"):
            pytest.skip("no thread in response")
        
        thread_id = exec_data["thread"]["thread_id"]

        # Submit kill command — should be rejected (thread is terminal)
        # but must NOT crash the daemon
        daemon_request(
            daemon, "POST", f"/threads/{thread_id}/commands",
            {"command_type": "kill"},
        )

        # Daemon must still be alive
        status, data = daemon_request(daemon, "GET", "/health")
        assert status == 200
        assert data["status"] == "ok"
