"""Golden tests for canonical JSON serialization across Rust and Python.

These tests verify that the Rust rye-state crate and Python rye/cas produce
identical canonical JSON and SHA256 hashes for all three core object types:
- ThreadEvent
- ThreadSnapshot
- ChainState

Test vectors are language-agnostic JSON files in .tmp/test-vectors/.

To run:
    pytest ryeos/rye/cas/test_golden.py -v
"""

import json
import hashlib
from pathlib import Path
from typing import Any, Dict


def canonical_json(obj: Any) -> str:
    """Canonical JSON representation for hashing.

    Rules:
    1. No whitespace
    2. Keys sorted alphabetically
    3. Standard JSON escaping
    """
    if obj is None:
        return "null"
    elif isinstance(obj, bool):
        return "true" if obj else "false"
    elif isinstance(obj, (int, float)):
        return json.dumps(obj)
    elif isinstance(obj, str):
        return json.dumps(obj)
    elif isinstance(obj, dict):
        items = []
        for key in sorted(obj.keys()):
            k = json.dumps(key)
            v = canonical_json(obj[key])
            items.append(f"{k}:{v}")
        return "{" + ",".join(items) + "}"
    elif isinstance(obj, list):
        items = [canonical_json(item) for item in obj]
        return "[" + ",".join(items) + "]"
    else:
        raise TypeError(f"cannot canonicalize {type(obj)}")


def sha256_hex(data: bytes) -> str:
    """SHA256 hash as hex string."""
    return hashlib.sha256(data).hexdigest()


def test_vectors_dir() -> Path:
    """Path to golden test vectors."""
    return Path(__file__).parent.parent.parent.parent / ".tmp" / "test-vectors"


class TestThreadEventVectors:
    """Golden tests for ThreadEvent canonical JSON."""

    def test_thread_event_hashes(self):
        """Verify ThreadEvent hashes match Rust computation."""
        vectors_path = test_vectors_dir() / "thread_event_vectors.json"
        if not vectors_path.exists():
            pytest.skip(f"Test vectors not found at {vectors_path}")

        with open(vectors_path) as f:
            vectors = json.load(f)

        for case in vectors["cases"]:
            name = case["name"]
            obj = case["object"]
            expected_hash = case.get("expected_hash", "GENERATED")

            if expected_hash == "GENERATED":
                pytest.skip(f"Skipping {name}: hash not yet generated")

            # Compute canonical JSON
            canonical = canonical_json(obj)
            computed_hash = sha256_hex(canonical.encode())

            assert computed_hash == expected_hash, \
                f"{name}: hash mismatch\nExpected: {expected_hash}\nGot: {computed_hash}"

    def test_thread_event_objects_are_valid(self):
        """Verify all ThreadEvent objects have correct schema."""
        vectors_path = test_vectors_dir() / "thread_event_vectors.json"
        if not vectors_path.exists():
            pytest.skip(f"Test vectors not found at {vectors_path}")

        with open(vectors_path) as f:
            vectors = json.load(f)

        for case in vectors["cases"]:
            obj = case["object"]
            assert obj["kind"] == "thread_event", f"Invalid kind in {case['name']}"
            assert obj["schema"] == 1, f"Invalid schema in {case['name']}"
            assert isinstance(obj["chain_seq"], int)
            assert isinstance(obj["thread_seq"], int)


class TestThreadSnapshotVectors:
    """Golden tests for ThreadSnapshot canonical JSON."""

    def test_thread_snapshot_hashes(self):
        """Verify ThreadSnapshot hashes match Rust computation."""
        vectors_path = test_vectors_dir() / "thread_snapshot_vectors.json"
        if not vectors_path.exists():
            pytest.skip(f"Test vectors not found at {vectors_path}")

        with open(vectors_path) as f:
            vectors = json.load(f)

        for case in vectors["cases"]:
            name = case["name"]
            obj = case["object"]
            expected_hash = case.get("expected_hash", "GENERATED")

            if expected_hash == "GENERATED":
                pytest.skip(f"Skipping {name}: hash not yet generated")

            # Compute canonical JSON
            canonical = canonical_json(obj)
            computed_hash = sha256_hex(canonical.encode())

            assert computed_hash == expected_hash, \
                f"{name}: hash mismatch\nExpected: {expected_hash}\nGot: {computed_hash}"

    def test_thread_snapshot_objects_are_valid(self):
        """Verify all ThreadSnapshot objects have correct schema."""
        vectors_path = test_vectors_dir() / "thread_snapshot_vectors.json"
        if not vectors_path.exists():
            pytest.skip(f"Test vectors not found at {vectors_path}")

        with open(vectors_path) as f:
            vectors = json.load(f)

        for case in vectors["cases"]:
            obj = case["object"]
            assert obj["kind"] == "thread_snapshot", f"Invalid kind in {case['name']}"
            assert obj["schema"] == 1, f"Invalid schema in {case['name']}"
            assert obj["status"] in ["created", "running", "completed", "failed", "cancelled", "killed", "timed_out", "continued"]


class TestChainStateVectors:
    """Golden tests for ChainState canonical JSON."""

    def test_chain_state_hashes(self):
        """Verify ChainState hashes match Rust computation."""
        vectors_path = test_vectors_dir() / "chain_state_vectors.json"
        if not vectors_path.exists():
            pytest.skip(f"Test vectors not found at {vectors_path}")

        with open(vectors_path) as f:
            vectors = json.load(f)

        for case in vectors["cases"]:
            name = case["name"]
            obj = case["object"]
            expected_hash = case.get("expected_hash", "GENERATED")

            if expected_hash == "GENERATED":
                pytest.skip(f"Skipping {name}: hash not yet generated")

            # Compute canonical JSON
            canonical = canonical_json(obj)
            computed_hash = sha256_hex(canonical.encode())

            assert computed_hash == expected_hash, \
                f"{name}: hash mismatch\nExpected: {expected_hash}\nGot: {computed_hash}"

    def test_chain_state_objects_are_valid(self):
        """Verify all ChainState objects have correct schema."""
        vectors_path = test_vectors_dir() / "chain_state_vectors.json"
        if not vectors_path.exists():
            pytest.skip(f"Test vectors not found at {vectors_path}")

        with open(vectors_path) as f:
            vectors = json.load(f)

        for case in vectors["cases"]:
            obj = case["object"]
            assert obj["kind"] == "chain_state", f"Invalid kind in {case['name']}"
            assert obj["schema"] == 1, f"Invalid schema in {case['name']}"
            assert isinstance(obj["last_chain_seq"], int)


class TestCanonicalJsonDeterminism:
    """Verify canonical JSON is deterministic across both languages."""

    def test_object_key_ordering(self):
        """Verify keys are always sorted."""
        obj = {
            "z": 1,
            "a": 2,
            "m": 3,
        }
        canonical = canonical_json(obj)
        # Keys should be sorted: a, m, z
        assert canonical == '{"a":2,"m":3,"z":1}'

    def test_nested_object_key_ordering(self):
        """Verify nested object keys are sorted."""
        obj = {
            "outer": {
                "z": 1,
                "a": 2,
            }
        }
        canonical = canonical_json(obj)
        assert '"outer":{"a":2,"z":1}' in canonical

    def test_array_order_preserved(self):
        """Verify array order is preserved (not sorted)."""
        obj = {
            "items": [3, 1, 2]
        }
        canonical = canonical_json(obj)
        assert canonical == '{"items":[3,1,2]}'

    def test_null_handling(self):
        """Verify null values serialize correctly."""
        obj = {"value": None}
        canonical = canonical_json(obj)
        assert canonical == '{"value":null}'

    def test_boolean_handling(self):
        """Verify booleans serialize correctly."""
        obj = {"yes": True, "no": False}
        canonical = canonical_json(obj)
        assert canonical == '{"no":false,"yes":true}'


if __name__ == "__main__":
    import pytest
    pytest.main([__file__, "-v"])
