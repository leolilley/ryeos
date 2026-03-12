"""Tests for snapshot cache and execution spaces."""

import pytest

from lillux.primitives import cas
from rye.cas.manifest import build_manifest
from rye.cas.objects import ProjectSnapshot
from rye.cas.store import cas_root
from rye.cas.checkout import (
    ensure_snapshot_cached,
    create_execution_space,
    ensure_user_space_cached,
    cleanup_execution_space,
)
from rye.constants import AI_DIR


def _make_project(root, files=None):
    if files is None:
        files = {
            ".ai/tools/my_tool.py": "print('hello')\n",
            ".ai/directives/my_dir.md": "# directive\ncontent here\n",
        }
    for rel_path, content in files.items():
        full = root / rel_path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)


def _make_user(root):
    files = {
        ".ai/config/agent/agent.yaml": "model: gpt-4\n",
    }
    for rel_path, content in files.items():
        full = root / rel_path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)


def _create_snapshot(project_root, cas_root_path):
    """Build manifest and store a ProjectSnapshot, return snapshot_hash."""
    ph, _ = build_manifest(project_root, "project")
    snapshot = ProjectSnapshot(
        project_manifest_hash=ph,
        user_manifest_hash="",
        source="push",
        timestamp="2026-01-01T00:00:00Z",
    )
    return cas.store_object(snapshot.to_dict(), cas_root_path)


class TestEnsureSnapshotCached:
    def test_materializes_snapshot(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project)
        root = cas_root(project)

        snapshot_hash = _create_snapshot(project, root)

        cache = tmp_path / "cache"
        cached = ensure_snapshot_cached(snapshot_hash, root, cache)

        assert cached.exists()
        assert (cached / ".snapshot_complete").exists()
        assert (cached / ".ai" / "tools" / "my_tool.py").exists()
        assert (cached / ".ai" / "tools" / "my_tool.py").read_text() == "print('hello')\n"

    def test_idempotent(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project)
        root = cas_root(project)

        snapshot_hash = _create_snapshot(project, root)

        cache = tmp_path / "cache"
        path1 = ensure_snapshot_cached(snapshot_hash, root, cache)
        path2 = ensure_snapshot_cached(snapshot_hash, root, cache)
        assert path1 == path2

    def test_staging_cleanup_on_reentry(self, tmp_path):
        """If a staging dir exists from a crashed attempt, it's cleaned up."""
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project)
        root = cas_root(project)

        snapshot_hash = _create_snapshot(project, root)

        cache = tmp_path / "cache"
        # Create a leftover staging dir
        staging = cache / "snapshots" / f".staging-{snapshot_hash}"
        staging.mkdir(parents=True)
        (staging / "garbage").write_text("leftover")

        cached = ensure_snapshot_cached(snapshot_hash, root, cache)
        assert cached.exists()
        assert not staging.exists()

    def test_missing_snapshot_raises(self, tmp_path):
        cache = tmp_path / "cache"
        root = tmp_path / "cas"
        root.mkdir()
        with pytest.raises(FileNotFoundError, match="ProjectSnapshot"):
            ensure_snapshot_cached("nonexistent", root, cache)


class TestCreateExecutionSpace:
    def test_creates_mutable_copy(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project)
        root = cas_root(project)

        snapshot_hash = _create_snapshot(project, root)

        cache = tmp_path / "cache"
        exec_root = tmp_path / "executions"
        exec_root.mkdir()

        exec_space = create_execution_space(
            snapshot_hash, "thread-123", root, cache, exec_root,
        )

        assert exec_space.exists()
        assert (exec_space / ".ai" / "tools" / "my_tool.py").exists()

        # Verify it's mutable (can write to it)
        (exec_space / ".ai" / "knowledge" / "new.md").parent.mkdir(parents=True, exist_ok=True)
        (exec_space / ".ai" / "knowledge" / "new.md").write_text("new knowledge")
        assert (exec_space / ".ai" / "knowledge" / "new.md").read_text() == "new knowledge"

        # Original cache is unchanged
        cached = cache / "snapshots" / snapshot_hash
        assert not (cached / ".ai" / "knowledge" / "new.md").exists()

    def test_cleanup(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project)
        root = cas_root(project)

        snapshot_hash = _create_snapshot(project, root)

        cache = tmp_path / "cache"
        exec_root = tmp_path / "executions"
        exec_root.mkdir()

        exec_space = create_execution_space(
            snapshot_hash, "thread-456", root, cache, exec_root,
        )
        assert exec_space.exists()

        cleanup_execution_space(exec_space)
        assert not exec_space.exists()


class TestEnsureUserSpaceCached:
    def test_materializes_user_space(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project)

        user = tmp_path / "user"
        user.mkdir()
        _make_user(user)

        root = cas_root(project)
        uh, _ = build_manifest(user, "user", project_path=project)

        cache = tmp_path / "cache"
        cached = ensure_user_space_cached(uh, root, cache)

        assert cached.exists()
        assert (cached / ".user_space_complete").exists()
        assert (cached / ".ai" / "config" / "agent" / "agent.yaml").exists()

    def test_idempotent(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project)

        user = tmp_path / "user"
        user.mkdir()
        _make_user(user)

        root = cas_root(project)
        uh, _ = build_manifest(user, "user", project_path=project)

        cache = tmp_path / "cache"
        path1 = ensure_user_space_cached(uh, root, cache)
        path2 = ensure_user_space_cached(uh, root, cache)
        assert path1 == path2
