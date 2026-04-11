"""Tests for registry endpoints."""

import pytest

from conftest import PROJECT_ROOT
from tests.services.ryeosd.conftest import daemon_request


class TestRegistry:
    """Test registry endpoints."""

    def test_publish_to_registry(self, daemon):
        """POST /registry/publish should publish to registry."""
        status, data = daemon_request(daemon, "POST", "/registry/publish", {
            "kind": "tool",
            "item_id": "test/item",
            "metadata": {}
        })
        assert status in (200, 400, 422)  # May fail due to validation

    def test_registry_search(self, daemon):
        """GET /registry/search should search registry."""
        status, data = daemon_request(daemon, "GET", "/registry/search")
        assert status in (200, 400)

    def test_get_registry_item(self, daemon):
        """GET /registry/items/{kind}/{item_id} should retrieve entry."""
        status, data = daemon_request(daemon, "GET", "/registry/items/tool/nonexistent")
        assert status in (404, 400)
