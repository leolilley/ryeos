"""Tests for graph input default application (_apply_input_defaults)."""

import importlib.util
import sys
from pathlib import Path

import pytest

# Load walker module from the core bundle
_WALKER_DIR = (
    Path(__file__).resolve().parents[3]
    / "ryeos" / "bundles" / "core" / "ryeos_core"
    / ".ai" / "tools" / "rye" / "core" / "runtimes" / "state-graph"
)

if str(_WALKER_DIR) not in sys.path:
    sys.path.insert(0, str(_WALKER_DIR))

_spec = importlib.util.spec_from_file_location("walker_defaults", _WALKER_DIR / "walker.py")
_walker = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_walker)

_apply_input_defaults = _walker._apply_input_defaults
_validate_inputs = _walker._validate_inputs


class TestApplyInputDefaults:
    """Test _apply_input_defaults fills missing params from config_schema."""

    def test_no_schema_returns_params_unchanged(self):
        params = {"a": 1}
        assert _apply_input_defaults(params, None) == {"a": 1}

    def test_empty_schema_returns_params_unchanged(self):
        params = {"a": 1}
        assert _apply_input_defaults(params, {}) == {"a": 1}

    def test_applies_missing_default(self):
        params = {}
        schema = {
            "type": "object",
            "properties": {
                "output_dir": {"type": "string", "default": "graph-output/cas-showcase"},
            },
        }
        result = _apply_input_defaults(params, schema)
        assert result == {"output_dir": "graph-output/cas-showcase"}

    def test_does_not_override_explicit_value(self):
        params = {"output_dir": "custom/path"}
        schema = {
            "type": "object",
            "properties": {
                "output_dir": {"type": "string", "default": "graph-output/default"},
            },
        }
        result = _apply_input_defaults(params, schema)
        assert result == {"output_dir": "custom/path"}

    def test_multiple_defaults_mixed(self):
        params = {"file_path": "src/main.py"}
        schema = {
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "output_dir": {"type": "string", "default": "graph-output"},
                "verbose": {"type": "boolean", "default": False},
            },
            "required": ["file_path"],
        }
        result = _apply_input_defaults(params, schema)
        assert result == {
            "file_path": "src/main.py",
            "output_dir": "graph-output",
            "verbose": False,
        }

    def test_no_properties_key(self):
        params = {"x": 1}
        schema = {"type": "object"}
        assert _apply_input_defaults(params, schema) == {"x": 1}

    def test_property_without_default_not_added(self):
        params = {}
        schema = {
            "type": "object",
            "properties": {
                "required_field": {"type": "string"},
            },
        }
        result = _apply_input_defaults(params, schema)
        assert result == {}

    def test_does_not_mutate_original_params(self):
        params = {"a": 1}
        schema = {
            "type": "object",
            "properties": {
                "b": {"type": "integer", "default": 2},
            },
        }
        _apply_input_defaults(params, schema)
        assert params == {"a": 1}


class TestValidateInputsWithDefaults:
    """Test that defaults + validation work together correctly."""

    def test_default_satisfies_required(self):
        schema = {
            "type": "object",
            "properties": {
                "output_dir": {"type": "string", "default": "out"},
            },
            "required": ["output_dir"],
        }
        params = _apply_input_defaults({}, schema)
        errors = _validate_inputs(params, schema)
        assert errors == []

    def test_missing_required_without_default_errors(self):
        schema = {
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
            },
            "required": ["file_path"],
        }
        params = _apply_input_defaults({}, schema)
        errors = _validate_inputs(params, schema)
        assert len(errors) == 1
        assert "file_path" in errors[0]
