"""Tests for install lockfile and uninstall verb."""

import json
import os
from pathlib import Path

import pytest

from rye.constants import AI_DIR


class TestLockfile:
    """Tests for bundle lockfile format and content."""

    def _make_lockfile(self, bundle_dir, **overrides):
        """Helper to create a install-receipt.json."""
        from datetime import datetime, timezone
        lock_data = {
            "bundle_id": "test-bundle",
            "version": "1.0.0",
            "manifest_hash": None,
            "installed_at": datetime.now(timezone.utc).isoformat(),
            "files": [],
        }
        lock_data.update(overrides)
        bundle_dir.mkdir(parents=True, exist_ok=True)
        lock_path = bundle_dir / "install-receipt.json"
        lock_path.write_text(json.dumps(lock_data, indent=2))
        return lock_path

    def test_lockfile_schema(self, tmp_path):
        """Lockfile contains required fields."""
        bundle_dir = tmp_path / AI_DIR / "bundles" / "test-bundle"
        lock_path = self._make_lockfile(bundle_dir)

        data = json.loads(lock_path.read_text())
        assert "bundle_id" in data
        assert "version" in data
        assert "installed_at" in data
        assert "files" in data
        assert isinstance(data["files"], list)

    def test_lockfile_records_files(self, tmp_path):
        """Lockfile records the list of installed files."""
        bundle_dir = tmp_path / AI_DIR / "bundles" / "test-bundle"
        files = [".ai/tools/my_tool.py", ".ai/directives/my_dir.md"]
        lock_path = self._make_lockfile(bundle_dir, files=files)

        data = json.loads(lock_path.read_text())
        assert data["files"] == files

    def test_lockfile_iso_timestamp(self, tmp_path):
        """installed_at is a valid ISO 8601 timestamp."""
        from datetime import datetime
        bundle_dir = tmp_path / AI_DIR / "bundles" / "test-bundle"
        lock_path = self._make_lockfile(bundle_dir)

        data = json.loads(lock_path.read_text())
        # Should parse without error
        dt = datetime.fromisoformat(data["installed_at"])
        assert dt is not None


class TestUninstallVerb:
    """Tests for the uninstall CLI verb handler logic."""

    def _install_bundle(self, root, bundle_id, files=None):
        """Simulate an installed bundle with lockfile and files."""
        if files is None:
            files = {
                f".ai/tools/{bundle_id}/hello.py": "print('hello')",
                f".ai/directives/{bundle_id}/setup.md": "# Setup\n",
            }

        # Write the files
        for rel_path, content in files.items():
            full_path = root / rel_path
            full_path.parent.mkdir(parents=True, exist_ok=True)
            full_path.write_text(content)

        # Write manifest
        bundle_dir = root / AI_DIR / "bundles" / bundle_id
        bundle_dir.mkdir(parents=True, exist_ok=True)
        (bundle_dir / "manifest.yaml").write_text(f"bundle_id: {bundle_id}\n")

        # Write lockfile
        lock_data = {
            "bundle_id": bundle_id,
            "version": "1.0.0",
            "manifest_hash": None,
            "installed_at": "2026-03-16T00:00:00+00:00",
            "files": list(files.keys()),
        }
        (bundle_dir / "install-receipt.json").write_text(json.dumps(lock_data, indent=2))

        return bundle_dir

    def test_uninstall_removes_files(self, tmp_path, monkeypatch):
        """Uninstall removes files listed in lockfile."""
        root = tmp_path / "user"
        root.mkdir()
        monkeypatch.setenv("USER_SPACE", str(root))

        files = {
            ".ai/tools/test-bundle/hello.py": "print('hello')",
        }
        self._install_bundle(root, "test-bundle", files)

        # Verify file exists before uninstall
        assert (root / ".ai/tools/test-bundle/hello.py").exists()

        # Import and run uninstall handler
        from unittest.mock import MagicMock
        from rye_cli.verbs.uninstall import _handle_uninstall

        args = MagicMock()
        args.bundle_id = "test-bundle"
        args.space = "user"

        _handle_uninstall(args, str(tmp_path / "project"))

        # File should be removed
        assert not (root / ".ai/tools/test-bundle/hello.py").exists()
        # Bundle dir should be removed
        assert not (root / AI_DIR / "bundles" / "test-bundle").exists()

    def test_uninstall_removes_bundle_dir(self, tmp_path, monkeypatch):
        """Uninstall removes the bundle directory itself."""
        root = tmp_path / "user"
        root.mkdir()
        monkeypatch.setenv("USER_SPACE", str(root))
        self._install_bundle(root, "test-bundle")

        from unittest.mock import MagicMock
        from rye_cli.verbs.uninstall import _handle_uninstall

        args = MagicMock()
        args.bundle_id = "test-bundle"
        args.space = "user"

        _handle_uninstall(args, str(tmp_path / "project"))

        assert not (root / AI_DIR / "bundles" / "test-bundle").exists()

    def test_uninstall_missing_bundle_prints_error(self, tmp_path, monkeypatch, capsys):
        """Uninstall of non-existent bundle prints error."""
        root = tmp_path / "user"
        root.mkdir()
        monkeypatch.setenv("USER_SPACE", str(root))

        from unittest.mock import MagicMock
        from rye_cli.verbs.uninstall import _handle_uninstall

        args = MagicMock()
        args.bundle_id = "nonexistent"
        args.space = "user"

        with pytest.raises(SystemExit):
            _handle_uninstall(args, str(tmp_path / "project"))

        output = capsys.readouterr().out
        assert "not installed" in output or "error" in output.lower()

    def test_uninstall_no_lockfile_still_removes_dir(self, tmp_path, monkeypatch):
        """Bundle dir without lockfile is still removed."""
        root = tmp_path / "user"
        root.mkdir()
        monkeypatch.setenv("USER_SPACE", str(root))

        bundle_dir = root / AI_DIR / "bundles" / "no-lock-bundle"
        bundle_dir.mkdir(parents=True)
        (bundle_dir / "manifest.yaml").write_text("bundle_id: no-lock-bundle\n")

        from unittest.mock import MagicMock
        from rye_cli.verbs.uninstall import _handle_uninstall

        args = MagicMock()
        args.bundle_id = "no-lock-bundle"
        args.space = "user"

        _handle_uninstall(args, str(tmp_path / "project"))

        assert not bundle_dir.exists()

    def test_uninstall_project_space(self, tmp_path, monkeypatch):
        """Uninstall works for project space."""
        project = tmp_path / "project"
        project.mkdir()
        monkeypatch.setenv("USER_SPACE", str(tmp_path / "user"))

        files = {
            ".ai/tools/proj-tool/run.py": "print('run')",
        }
        self._install_bundle(project, "proj-tool", files)

        from unittest.mock import MagicMock
        from rye_cli.verbs.uninstall import _handle_uninstall

        args = MagicMock()
        args.bundle_id = "proj-tool"
        args.space = "project"

        _handle_uninstall(args, str(project))

        assert not (project / ".ai/tools/proj-tool/run.py").exists()
        assert not (project / AI_DIR / "bundles" / "proj-tool").exists()

    def test_uninstall_cleans_empty_parent_dirs(self, tmp_path, monkeypatch):
        """Uninstall cleans up empty parent directories after file removal."""
        root = tmp_path / "user"
        root.mkdir()
        monkeypatch.setenv("USER_SPACE", str(root))

        files = {
            ".ai/tools/deep/nested/tool.py": "print('deep')",
        }
        self._install_bundle(root, "cleanup-test", files)

        from unittest.mock import MagicMock
        from rye_cli.verbs.uninstall import _handle_uninstall

        args = MagicMock()
        args.bundle_id = "cleanup-test"
        args.space = "user"

        _handle_uninstall(args, str(tmp_path / "project"))

        # The deep/nested/ dirs should be cleaned up
        assert not (root / ".ai/tools/deep/nested").exists()
        assert not (root / ".ai/tools/deep").exists()

    def test_uninstall_cleans_empty_bundles_dir(self, tmp_path, monkeypatch):
        """If no bundles remain, the bundles/ dir is removed."""
        root = tmp_path / "user"
        root.mkdir()
        monkeypatch.setenv("USER_SPACE", str(root))
        self._install_bundle(root, "last-bundle", files={})

        from unittest.mock import MagicMock
        from rye_cli.verbs.uninstall import _handle_uninstall

        args = MagicMock()
        args.bundle_id = "last-bundle"
        args.space = "user"

        _handle_uninstall(args, str(tmp_path / "project"))

        assert not (root / AI_DIR / "bundles").exists()
