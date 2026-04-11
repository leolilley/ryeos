"""Tests for /health, /status, /public-key endpoints."""

import pytest

from conftest import PROJECT_ROOT
from tests.services.ryeosd.conftest import daemon_request


class TestHealth:
    def test_health_ok(self, daemon):
        status, data = daemon_request(daemon, "GET", "/health")
        assert status == 200
        assert data["status"] == "ok"

    def test_status_has_version(self, daemon):
        status, data = daemon_request(daemon, "GET", "/status")
        assert status == 200
        assert "version" in data
        assert "uptime_seconds" in data

    def test_public_key_returns_identity(self, daemon):
        status, data = daemon_request(daemon, "GET", "/public-key")
        assert status == 200
        assert data["kind"] == "identity/v1"
        assert data["principal_id"].startswith("fp:")
        assert "_signature" in data
