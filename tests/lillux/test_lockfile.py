"""Tests for lockfile I/O (Phase 2.1)."""

import json
from pathlib import Path
import tempfile
from datetime import datetime

import pytest
from lillux.primitives.lockfile import LockfileRoot, Lockfile, LockfileManager
from lillux.primitives.errors import LockfileError


class TestLockfileRoot:
    """Test LockfileRoot dataclass."""

    def test_create_lockfile_root(self):
        """Create LockfileRoot with tool_id, version, integrity."""
        root = LockfileRoot(
            tool_id="my_tool",
            version="1.0.0",
            integrity="abc123def456",
        )
        assert root.tool_id == "my_tool"
        assert root.version == "1.0.0"
        assert root.integrity == "abc123def456"


class TestLockfile:
    """Test Lockfile dataclass."""

    def test_create_lockfile(self):
        """Create Lockfile with required fields."""
        root = LockfileRoot("tool", "1.0.0", "hash")
        lockfile = Lockfile(
            lockfile_version="1.0",
            generated_at="2024-01-01T00:00:00Z",
            root=root,
            resolved_chain=[],
        )
        assert lockfile.lockfile_version == "1.0"
        assert lockfile.generated_at == "2024-01-01T00:00:00Z"
        assert lockfile.root.tool_id == "tool"
        assert lockfile.resolved_chain == []

    def test_lockfile_with_registry(self):
        """Lockfile can include optional registry field."""
        root = LockfileRoot("tool", "1.0.0", "hash")
        registry = {"tools": {"my_tool": "url"}}
        lockfile = Lockfile(
            lockfile_version="1.0",
            generated_at="2024-01-01T00:00:00Z",
            root=root,
            resolved_chain=[],
            registry=registry,
        )
        assert lockfile.registry == registry


class TestLockfileManager:
    """Test LockfileManager API."""

    def test_load_valid_lockfile(self):
        """Load valid lockfile from JSON file."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "lockfile.json"
            lockfile_data = {
                "lockfile_version": "1.0",
                "generated_at": "2024-01-01T00:00:00Z",
                "root": {
                    "tool_id": "test_tool",
                    "version": "1.0.0",
                    "integrity": "abc123",
                },
                "resolved_chain": [],
                "registry": None,
            }
            lockfile_path.write_text(json.dumps(lockfile_data))

            manager = LockfileManager()
            lockfile = manager.load(lockfile_path)
            assert lockfile.lockfile_version == "1.0"
            assert lockfile.root.tool_id == "test_tool"

    def test_load_nonexistent_file_raises(self):
        """Loading nonexistent file raises FileNotFoundError."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "missing.json"
            manager = LockfileManager()
            with pytest.raises(FileNotFoundError):
                manager.load(lockfile_path)

    def test_load_invalid_json_raises_lockfile_error(self):
        """Loading invalid JSON raises LockfileError."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "invalid.json"
            lockfile_path.write_text("not valid json {")

            manager = LockfileManager()
            with pytest.raises(LockfileError):
                manager.load(lockfile_path)

    def test_load_missing_required_fields_raises_lockfile_error(self):
        """Loading file with missing required fields raises LockfileError."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "incomplete.json"
            lockfile_data = {
                "lockfile_version": "1.0",
                # missing generated_at, root, resolved_chain
            }
            lockfile_path.write_text(json.dumps(lockfile_data))

            manager = LockfileManager()
            with pytest.raises(LockfileError):
                manager.load(lockfile_path)

    def test_save_lockfile(self):
        """Save lockfile to JSON file."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "output.json"
            root = LockfileRoot("tool", "1.0.0", "hash")
            lockfile = Lockfile(
                lockfile_version="1.0",
                generated_at="2024-01-01T00:00:00Z",
                root=root,
                resolved_chain=[],
            )

            manager = LockfileManager()
            saved_path = manager.save(lockfile, lockfile_path)

            assert saved_path == lockfile_path
            assert lockfile_path.exists()

            # Verify saved content
            saved_data = json.loads(lockfile_path.read_text())
            assert saved_data["lockfile_version"] == "1.0"
            assert saved_data["root"]["tool_id"] == "tool"

    def test_save_does_not_create_parent_dirs(self):
        """Save does not create parent directories."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "nonexistent" / "dir" / "lockfile.json"
            root = LockfileRoot("tool", "1.0.0", "hash")
            lockfile = Lockfile(
                lockfile_version="1.0",
                generated_at="2024-01-01T00:00:00Z",
                root=root,
                resolved_chain=[],
            )

            manager = LockfileManager()
            with pytest.raises(FileNotFoundError):
                manager.save(lockfile, lockfile_path)

    def test_exists_returns_true_for_existing_file(self):
        """exists() returns True for existing lockfile."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "exists.json"
            lockfile_path.write_text("{}")

            manager = LockfileManager()
            assert manager.exists(lockfile_path) is True

    def test_exists_returns_false_for_missing_file(self):
        """exists() returns False for missing lockfile."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "missing.json"

            manager = LockfileManager()
            assert manager.exists(lockfile_path) is False

    def test_roundtrip_lockfile(self):
        """Save and load roundtrip preserves lockfile."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "roundtrip.json"

            root = LockfileRoot(
                tool_id="my_tool",
                version="1.2.3",
                integrity="abc123def456",
            )
            original = Lockfile(
                lockfile_version="1.0",
                generated_at="2024-01-01T12:00:00Z",
                root=root,
                resolved_chain=["dep1", "dep2"],
                registry={"key": "value"},
            )

            manager = LockfileManager()
            manager.save(original, lockfile_path)
            loaded = manager.load(lockfile_path)

            assert loaded.lockfile_version == original.lockfile_version
            assert loaded.generated_at == original.generated_at
            assert loaded.root.tool_id == original.root.tool_id
            assert loaded.root.version == original.root.version
            assert loaded.root.integrity == original.root.integrity
            assert loaded.resolved_chain == original.resolved_chain
            assert loaded.registry == original.registry

    def test_load_with_extra_fields_ignored(self):
        """Loading file with extra fields ignores them gracefully."""
        with tempfile.TemporaryDirectory() as tmpdir:
            lockfile_path = Path(tmpdir) / "extra_fields.json"
            lockfile_data = {
                "lockfile_version": "1.0",
                "generated_at": "2024-01-01T00:00:00Z",
                "root": {
                    "tool_id": "tool",
                    "version": "1.0.0",
                    "integrity": "hash",
                },
                "resolved_chain": [],
                "registry": None,
                "extra_field": "should be ignored",
            }
            lockfile_path.write_text(json.dumps(lockfile_data))

            manager = LockfileManager()
            lockfile = manager.load(lockfile_path)
            assert lockfile.root.tool_id == "tool"
