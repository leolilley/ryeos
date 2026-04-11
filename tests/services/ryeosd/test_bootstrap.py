"""Tests for daemon initialization and bootstrap."""

import pytest

from conftest import PROJECT_ROOT
from tests.services.ryeosd.conftest import daemon_request


class TestBootstrap:
    """Test daemon initialization."""

    def test_state_dir_exists(self, daemon):
        """State directory should be created."""
        assert daemon["state_dir"].exists()

    def test_database_exists(self, daemon):
        """Database should be created."""
        db_path = daemon["state_dir"] / "db" / "ryeosd.sqlite3"
        assert db_path.exists()

    def test_cas_root_exists(self, daemon):
        """CAS root should be created."""
        assert daemon["cas_root"].exists()

    def test_daemon_healthy(self, daemon):
        """Daemon should be healthy and responding."""
        status, data = daemon_request(daemon, "GET", "/health")
        assert status == 200
        assert data["status"] == "ok"
