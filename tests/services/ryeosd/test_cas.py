"""Tests for CAS (Content-Addressable Storage) endpoints."""

import pytest

from conftest import PROJECT_ROOT
from tests.services.ryeosd.conftest import daemon_request


class TestCASObjects:
    """Test /objects endpoints (has, put, get)."""

    def test_has_objects(self, daemon):
        """POST /objects/has should check for objects."""
        status, data = daemon_request(daemon, "POST", "/objects/has", {
            "object_ids": []
        })
        assert status in (200, 400, 422)

    def test_put_objects(self, daemon):
        """PUT objects."""
        status, data = daemon_request(daemon, "POST", "/objects/put", {
            "objects": []
        })
        assert status in (200, 400, 422)

    def test_get_objects(self, daemon):
        """GET objects."""
        status, data = daemon_request(daemon, "POST", "/objects/get", {
            "object_ids": []
        })
        assert status in (200, 400, 422)
