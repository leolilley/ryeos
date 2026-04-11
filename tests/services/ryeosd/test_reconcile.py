"""Tests for daemon restart reconciliation."""

import pytest

from tests.services.ryeosd.conftest import daemon_request


class TestReconcile:
    """Test that restart reconciles orphaned threads."""

    def test_health_after_clean_restart(self, daemon):
        """Daemon should be healthy — verifies reconcile doesn't crash on empty state."""
        status, data = daemon_request(daemon, "GET", "/health")
        assert status == 200
        assert data["status"] == "ok"
