"""Tests for schema validator (Phase 5.1)."""

import pytest
from lillux.schemas.schema_validator import validate_parameters, ValidationResult


class TestValidationResult:
    """Test ValidationResult dataclass."""

    def test_validation_result_valid(self):
        """ValidationResult with valid=True."""
        result = ValidationResult(valid=True, errors=[])
        assert result.valid is True
        assert result.errors == []

    def test_validation_result_invalid(self):
        """ValidationResult with valid=False and errors."""
        errors = ["field1 is required", "field2 must be string"]
        result = ValidationResult(valid=False, errors=errors)
        assert result.valid is False
        assert len(result.errors) == 2


class TestValidateParameters:
    """Test validate_parameters function."""

    def test_validate_simple_type_string(self):
        """Validate string type."""
        schema = {"type": "string"}
        result = validate_parameters("hello", schema)
        assert result.valid is True

    def test_validate_simple_type_number(self):
        """Validate number type."""
        schema = {"type": "number"}
        result = validate_parameters(42.5, schema)
        assert result.valid is True

    def test_validate_simple_type_integer(self):
        """Validate integer type."""
        schema = {"type": "integer"}
        result = validate_parameters(42, schema)
        assert result.valid is True

    def test_validate_simple_type_boolean(self):
        """Validate boolean type."""
        schema = {"type": "boolean"}
        result = validate_parameters(True, schema)
        assert result.valid is True

    def test_validate_simple_type_array(self):
        """Validate array type."""
        schema = {"type": "array"}
        result = validate_parameters([1, 2, 3], schema)
        assert result.valid is True

    def test_validate_simple_type_object(self):
        """Validate object type."""
        schema = {"type": "object"}
        result = validate_parameters({"key": "value"}, schema)
        assert result.valid is True

    def test_validate_wrong_type_fails(self):
        """Wrong type fails validation."""
        schema = {"type": "string"}
        result = validate_parameters(42, schema)
        assert result.valid is False
        assert len(result.errors) > 0

    def test_validate_required_fields(self):
        """Validate required fields in object."""
        schema = {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "integer"},
            },
            "required": ["name"],
        }
        result = validate_parameters({"name": "Alice"}, schema)
        assert result.valid is True

    def test_validate_missing_required_field(self):
        """Missing required field fails."""
        schema = {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "email": {"type": "string"},
            },
            "required": ["email"],
        }
        result = validate_parameters({"name": "Alice"}, schema)
        assert result.valid is False

    def test_validate_enum_values(self):
        """Validate enum constraint."""
        schema = {"enum": ["red", "green", "blue"]}
        result = validate_parameters("red", schema)
        assert result.valid is True

    def test_validate_enum_invalid_value(self):
        """Invalid enum value fails."""
        schema = {"enum": ["red", "green", "blue"]}
        result = validate_parameters("yellow", schema)
        assert result.valid is False

    def test_validate_minimum_number(self):
        """Validate minimum constraint on number."""
        schema = {"type": "number", "minimum": 0}
        result = validate_parameters(5.5, schema)
        assert result.valid is True

    def test_validate_number_below_minimum(self):
        """Number below minimum fails."""
        schema = {"type": "number", "minimum": 0}
        result = validate_parameters(-5, schema)
        assert result.valid is False

    def test_validate_maximum_number(self):
        """Validate maximum constraint on number."""
        schema = {"type": "number", "maximum": 100}
        result = validate_parameters(50, schema)
        assert result.valid is True

    def test_validate_number_above_maximum(self):
        """Number above maximum fails."""
        schema = {"type": "number", "maximum": 100}
        result = validate_parameters(150, schema)
        assert result.valid is False

    def test_validate_minItems_array(self):
        """Validate minItems constraint on array."""
        schema = {"type": "array", "minItems": 2}
        result = validate_parameters([1, 2, 3], schema)
        assert result.valid is True

    def test_validate_array_too_few_items(self):
        """Array with fewer items than minItems fails."""
        schema = {"type": "array", "minItems": 2}
        result = validate_parameters([1], schema)
        assert result.valid is False

    def test_validate_maxItems_array(self):
        """Validate maxItems constraint on array."""
        schema = {"type": "array", "maxItems": 3}
        result = validate_parameters([1, 2], schema)
        assert result.valid is True

    def test_validate_array_too_many_items(self):
        """Array with more items than maxItems fails."""
        schema = {"type": "array", "maxItems": 3}
        result = validate_parameters([1, 2, 3, 4], schema)
        assert result.valid is False

    def test_validate_pattern_string(self):
        """Validate pattern constraint on string."""
        schema = {"type": "string", "pattern": "^[a-z]+$"}
        result = validate_parameters("hello", schema)
        assert result.valid is True

    def test_validate_pattern_mismatch(self):
        """String not matching pattern fails."""
        schema = {"type": "string", "pattern": "^[0-9]+$"}
        result = validate_parameters("hello", schema)
        assert result.valid is False

    def test_validate_items_schema_array(self):
        """Validate items schema for array elements."""
        schema = {
            "type": "array",
            "items": {"type": "string"},
        }
        result = validate_parameters(["a", "b", "c"], schema)
        assert result.valid is True

    def test_validate_array_items_wrong_type(self):
        """Array with wrong item type fails."""
        schema = {
            "type": "array",
            "items": {"type": "integer"},
        }
        result = validate_parameters([1, 2, "three"], schema)
        assert result.valid is False

    def test_validate_default_value_applied(self):
        """Default values are supported in schema."""
        schema = {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "integer", "default": 18},
            },
        }
        parameters = {"name": "Alice"}
        result = validate_parameters(parameters, schema)
        assert result.valid is True

    def test_validate_nested_object(self):
        """Validate nested object structure."""
        schema = {
            "type": "object",
            "properties": {
                "user": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "email": {"type": "string"},
                    },
                    "required": ["name"],
                },
            },
        }
        data = {"user": {"name": "Alice", "email": "alice@example.com"}}
        result = validate_parameters(data, schema)
        assert result.valid is True

    def test_validate_null_type(self):
        """Validate null type."""
        schema = {"type": "null"}
        result = validate_parameters(None, schema)
        assert result.valid is True

    def test_validate_multiple_properties(self):
        """Validate object with multiple properties."""
        schema = {
            "type": "object",
            "properties": {
                "id": {"type": "integer"},
                "name": {"type": "string"},
                "active": {"type": "boolean"},
                "tags": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["id", "name"],
        }
        data = {
            "id": 1,
            "name": "Test",
            "active": True,
            "tags": ["tag1", "tag2"],
        }
        result = validate_parameters(data, schema)
        assert result.valid is True
