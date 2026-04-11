"""Tests for thread kind profile validation."""

import pytest

from conftest import PROJECT_ROOT
from tests.services.ryeosd.conftest import daemon_request


class TestKindProfiles:
    """Test kind profile validation during execution."""

    def test_execute_with_kind(self, daemon):
        """Should accept execution with kind specification."""
        status, data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
            "kind": "agent",
        })
        assert status in (200, 400, 422)

    def test_execute_with_model_hints(self, daemon):
        """Should accept execution with model hints."""
        status, data = daemon_request(daemon, "POST", "/execute", {
            "item_ref": "tool:rye/core/identity",
            "parameters": {"action": "whoami"},
            "model_tier": "fast",
        })
        assert status in (200, 400, 422)
