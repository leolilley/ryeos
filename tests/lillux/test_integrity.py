"""Tests for integrity hashing primitives."""

import json

import pytest

from lillux.primitives.integrity import (
    canonical_json,
    compute_integrity,
)


class TestCanonicalJson:
    """Test canonical_json serialization."""

    def test_sorted_keys(self):
        """Keys are sorted."""
        result = canonical_json({"b": 2, "a": 1})
        assert result == '{"a":1,"b":2}'

    def test_no_whitespace(self):
        """No extra whitespace in output."""
        result = canonical_json({"key": "value"})
        assert " " not in result

    def test_nested_sorted(self):
        """Nested dict keys are sorted."""
        result = canonical_json({"outer": {"b": 2, "a": 1}})
        parsed = json.loads(result)
        assert list(parsed["outer"].keys()) == ["a", "b"]


class TestComputeIntegrity:
    """Test compute_integrity with arbitrary data dicts."""

    def test_deterministic_same_input_same_hash(self):
        """Same input produces same hash."""
        data = {"id": "my_tool", "version": "1.0.0", "manifest": {"name": "test_tool"}}
        hash1 = compute_integrity(data)
        hash2 = compute_integrity(data)
        assert hash1 == hash2

    def test_hash_is_64_char_hex(self):
        """Hash is SHA256 hex (64 chars)."""
        h = compute_integrity({"id": "test", "version": "1.0.0"})
        assert len(h) == 64
        assert all(c in "0123456789abcdef" for c in h)

    def test_different_ids_different_hash(self):
        """Different field values produce different hashes."""
        h1 = compute_integrity({"id": "tool1", "version": "1.0.0"})
        h2 = compute_integrity({"id": "tool2", "version": "1.0.0"})
        assert h1 != h2

    def test_different_versions_different_hash(self):
        """Different versions produce different hashes."""
        h1 = compute_integrity({"id": "x", "version": "1.0.0"})
        h2 = compute_integrity({"id": "x", "version": "1.0.1"})
        assert h1 != h2

    def test_different_fields_different_hash(self):
        """Extra fields change the hash."""
        h1 = compute_integrity({"id": "x", "version": "1.0.0"})
        h2 = compute_integrity({"id": "x", "version": "1.0.0", "extra": "field"})
        assert h1 != h2

    def test_key_order_irrelevant(self):
        """Dict key order doesn't affect hash (canonical JSON)."""
        h1 = compute_integrity({"a": 1, "b": 2})
        h2 = compute_integrity({"b": 2, "a": 1})
        assert h1 == h2

    def test_nested_dict_order_irrelevant(self):
        """Nested dict key order doesn't affect hash."""
        h1 = compute_integrity({"config": {"x": 1, "y": 2}})
        h2 = compute_integrity({"config": {"y": 2, "x": 1}})
        assert h1 == h2

    def test_empty_dict(self):
        """Empty dict produces valid hash."""
        h = compute_integrity({})
        assert len(h) == 64

    def test_none_values(self):
        """None values are handled."""
        h = compute_integrity({"key": None})
        assert len(h) == 64

    def test_lists(self):
        """Lists are handled."""
        h = compute_integrity({"items": [1, 2, 3]})
        assert len(h) == 64

    def test_complex_nested_structure(self):
        """Complex nested structures are deterministic."""
        data = {
            "config": {
                "env": {"VAR1": "val1", "VAR2": "val2"},
                "args": [1, 2, 3],
            },
            "metadata": {"author": "test", "tags": ["a", "b"]},
        }
        h1 = compute_integrity(data)
        h2 = compute_integrity(data)
        assert h1 == h2

    def test_tool_style_data(self):
        """Works with tool-like data (caller structures it)."""
        data = {
            "tool_id": "my_tool",
            "version": "1.0.0",
            "manifest": {"name": "test"},
            "files": [{"path": "script.py", "sha256": "abc123"}],
        }
        h = compute_integrity(data)
        assert len(h) == 64

    def test_directive_style_data(self):
        """Works with directive-like data (caller structures it)."""
        data = {
            "directive_name": "test_directive",
            "version": "1.0.0",
            "xml_content": "<directive><name>test</name></directive>",
        }
        h = compute_integrity(data)
        assert len(h) == 64

    def test_knowledge_style_data(self):
        """Works with knowledge-like data (caller structures it)."""
        data = {
            "id": "zettel_1",
            "version": "1.0.0",
            "content": "This is knowledge",
        }
        h = compute_integrity(data)
        assert len(h) == 64
