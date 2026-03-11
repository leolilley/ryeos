"""Tests for config content schema validation.

Tests the data-driven config content validation pipeline:
- .config-schema.yaml tools declare target_config and schema
- rye.utils.config_validators discovers, caches, and validates
"""

import pytest

from rye.utils.config_validators import (
    validate_config_content,
    clear_config_schemas_cache,
)


@pytest.fixture(autouse=True)
def _clear_cache():
    """Clear schema cache between tests."""
    clear_config_schemas_cache()
    yield
    clear_config_schemas_cache()


class TestConfigSchemas:
    """Test content schema validation logic."""

    def test_valid_agent_config(self):
        result = validate_config_content("agent/agent", {
            "schema_version": "1.0.0",
            "provider": {"default": "anthropic"},
            "max_output_tokens": 16384,
        })
        assert result["valid"]
        assert result["issues"] == []

    def test_invalid_agent_config_type(self):
        result = validate_config_content("agent/agent", {
            "max_output_tokens": "not_a_number",
        })
        assert not result["valid"]
        assert len(result["issues"]) == 1
        assert "max_output_tokens" in result["issues"][0]
        assert "integer" in result["issues"][0]

    def test_valid_resilience_config(self):
        result = validate_config_content("agent/resilience", {
            "retry": {"max_retries": 3, "policies": {}},
            "limits": {
                "defaults": {"turns": 25, "tokens": 200000, "spend": 1.0},
                "enforcement": {"check_before_turn": True, "on_exceed": "escalate"},
            },
            "concurrency": {"max_concurrent_children": 5, "max_total_threads": 20},
        })
        assert result["valid"]
        assert result["issues"] == []

    def test_invalid_resilience_nested_type(self):
        result = validate_config_content("agent/resilience", {
            "retry": {"max_retries": "three"},
        })
        assert not result["valid"]
        assert len(result["issues"]) == 1
        assert "max_retries" in result["issues"][0]

    def test_valid_coordination_config(self):
        result = validate_config_content("agent/coordination", {
            "coordination": {
                "wait_threads": {"default_timeout": 600, "max_timeout": 3600},
                "continuation": {"trigger_threshold": 0.9, "summary_directive": "test"},
                "transcript_integrity": "strict",
                "orphan_detection": {"enabled": True, "stale_threshold_minutes": 60},
            },
        })
        assert result["valid"]
        assert result["issues"] == []

    def test_unknown_config_passes(self):
        """Config files without a schema should pass validation."""
        result = validate_config_content("agent/unknown", {"anything": "goes"})
        assert result["valid"]
        assert result["issues"] == []

    def test_null_values_accepted(self):
        """Null values should be accepted for any field type."""
        result = validate_config_content("agent/agent", {
            "provider": {"default": None},
            "max_output_tokens": None,
        })
        assert result["valid"]
        assert result["issues"] == []

    def test_empty_config_passes(self):
        """Empty config is valid — all fields are optional."""
        result = validate_config_content("agent/agent", {})
        assert result["valid"]
        assert result["issues"] == []

    def test_extra_keys_allowed(self):
        """User/project can add extra keys not in schema."""
        result = validate_config_content("agent/agent", {
            "schema_version": "1.0.0",
            "custom_field": "allowed",
        })
        assert result["valid"]
        assert result["issues"] == []

    def test_cas_remote_schema(self):
        """Core bundle config schema discovered via target_config."""
        result = validate_config_content("cas/remote", {
            "project_name": "test-project",
            "remotes": {"default": {"url": "https://x.com", "key_env": "K"}},
            "sync": {"include": [".ai/"], "exclude": [".git/"]},
        })
        assert result["valid"]
        assert result["issues"] == []

    def test_cas_remote_invalid_type(self):
        result = validate_config_content("cas/remote", {
            "project_name": 123,
        })
        assert not result["valid"]
        assert "project_name" in result["issues"][0]

    def test_cache_scoped_by_project_path(self, tmp_path):
        """Different project_path values get independent cache entries."""
        r1 = validate_config_content("cas/remote", {"project_name": 123})
        r2 = validate_config_content("cas/remote", {"project_name": 123}, project_path=tmp_path)
        assert not r1["valid"]
        assert not r2["valid"]
