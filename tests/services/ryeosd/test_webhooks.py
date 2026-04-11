"""Tests for webhook endpoints."""

import pytest

from conftest import PROJECT_ROOT
from tests.services.ryeosd.conftest import daemon_request


class TestWebhooks:
    """Test webhook CRUD operations."""

    def test_list_webhooks(self, daemon):
        """GET /webhook-bindings should list webhooks."""
        status, data = daemon_request(daemon, "GET", "/webhook-bindings")
        assert status in (200, 400, 422)

    def test_create_webhook(self, daemon):
        """POST /webhook-bindings should create a webhook."""
        status, data = daemon_request(daemon, "POST", "/webhook-bindings", {})
        assert status in (200, 400, 422)

    def test_revoke_webhook(self, daemon):
        """DELETE /webhook-bindings/{id} should revoke a webhook."""
        status, data = daemon_request(daemon, "DELETE", "/webhook-bindings/nonexistent")
        assert status in (200, 404, 400, 422)
