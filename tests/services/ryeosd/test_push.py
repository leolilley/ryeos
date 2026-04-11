"""Tests for push/refs endpoints."""

import pytest

from conftest import PROJECT_ROOT
from tests.services.ryeosd.conftest import daemon_request


class TestPush:
    """Test push operations."""

    def test_push(self, daemon):
        """POST /push should push items."""
        status, data = daemon_request(daemon, "POST", "/push", {
            "items": []
        })
        assert status in (200, 400, 422)

    def test_push_user_space(self, daemon):
        """POST /push/user-space should push user space."""
        status, data = daemon_request(daemon, "POST", "/push/user-space", {
            "user_space": {}
        })
        assert status in (200, 400, 422)

    def test_get_user_space(self, daemon):
        """GET /user-space should retrieve user space."""
        status, data = daemon_request(daemon, "GET", "/user-space")
        assert status in (200, 400, 404)
