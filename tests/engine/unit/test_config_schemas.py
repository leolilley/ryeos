"""Tests for config content schema validation (Step 4b).

Tests the data-driven config content validation pipeline:
- .config-schema.yaml tools define schemas (data)
- rye.utils.config_validators loads and validates (engine)
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
        result = validate_config_content("agent.yaml", {
            "schema_version": "1.0.0",
            "provider": {"default": "anthropic"},
            "max_output_tokens": 16384,
        })
        assert result["valid"]
        assert result["issues"] == []

    def test_invalid_agent_config_type(self):
        result = validate_config_content("agent.yaml", {
            "max_output_tokens": "not_a_number",
        })
        assert not result["valid"]
        assert len(result["issues"]) == 1
        assert "max_output_tokens" in result["issues"][0]
        assert "integer" in result["issues"][0]

    def test_valid_resilience_config(self):
        result = validate_config_content("resilience.yaml", {
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
        result = validate_config_content("resilience.yaml", {
            "retry": {"max_retries": "three"},
        })
        assert not result["valid"]
        assert len(result["issues"]) == 1
        assert "max_retries" in result["issues"][0]

    def test_valid_coordination_config(self):
        result = validate_config_content("coordination.yaml", {
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
        result = validate_config_content("unknown.yaml", {"anything": "goes"})
        assert result["valid"]
        assert result["issues"] == []

    def test_null_values_accepted(self):
        """Null values should be accepted for any field type."""
        result = validate_config_content("agent.yaml", {
            "provider": {"default": None},
            "max_output_tokens": None,
        })
        assert result["valid"]
        assert result["issues"] == []

    def test_empty_config_passes(self):
        """Empty config is valid — all fields are optional."""
        result = validate_config_content("agent.yaml", {})
        assert result["valid"]
        assert result["issues"] == []

    def test_extra_keys_allowed(self):
        """User/project can add extra keys not in schema."""
        result = validate_config_content("agent.yaml", {
            "schema_version": "1.0.0",
            "custom_field": "allowed",
        })
        assert result["valid"]
        assert result["issues"] == []
