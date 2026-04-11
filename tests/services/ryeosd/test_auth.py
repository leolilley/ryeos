"""Tests for auth middleware.

The test daemon starts with --require-auth disabled, so these tests verify
the auth-skip path and the public endpoint allowlist.
"""

import pytest

from tests.services.ryeosd.conftest import daemon_request


class TestPublicEndpoints:
    """Public endpoints should work without auth headers."""

    def test_health_no_auth(self, daemon):
        """GET /health is always public."""
        status, data = daemon_request(daemon, "GET", "/health")
        assert status == 200
        assert data["status"] == "ok"

    def test_status_no_auth(self, daemon):
        """GET /status is always public."""
        status, data = daemon_request(daemon, "GET", "/status")
        assert status == 200

    def test_public_key_no_auth(self, daemon):
        """GET /public-key is always public."""
        status, data = daemon_request(daemon, "GET", "/public-key")
        assert status == 200
        assert data["kind"] == "identity/v1"

    def test_require_auth_disabled(self, daemon):
        """Test daemon has auth disabled — private endpoints work without auth."""
        status, data = daemon_request(daemon, "GET", "/threads")
        assert status == 200
