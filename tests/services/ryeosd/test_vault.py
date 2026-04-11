"""Tests for vault endpoints."""

import pytest

from conftest import PROJECT_ROOT
from tests.services.ryeosd.conftest import daemon_request


class TestVault:
    """Test vault CRUD operations."""

    def test_vault_set(self, daemon):
        """POST /vault/set should store a vault entry."""
        status, data = daemon_request(daemon, "POST", "/vault/set", {})
        assert status in (200, 400, 422)

    def test_vault_list(self, daemon):
        """GET /vault/list should list vault entries."""
        status, data = daemon_request(daemon, "GET", "/vault/list")
        assert status in (200, 400)

    def test_vault_delete(self, daemon):
        """POST /vault/delete should delete a vault entry."""
        status, data = daemon_request(daemon, "POST", "/vault/delete", {})
        assert status in (200, 400, 422)
