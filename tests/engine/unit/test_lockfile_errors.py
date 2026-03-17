"""Tests for lockfile error messages including the lockfile path."""

import json
import tempfile
from pathlib import Path

import pytest

from rye.executor.lockfile_resolver import LockfileResolver
from lillux.primitives.lockfile import Lockfile, LockfileRoot


@pytest.fixture
def lockfile_env(tmp_path):
    """Set up project/user dirs with a lockfile resolver."""
    project = tmp_path / "project"
    user = tmp_path / "user"
    lockfiles_dir = project / ".ai" / "lockfiles"
    lockfiles_dir.mkdir(parents=True)
    (user / ".ai" / "lockfiles").mkdir(parents=True)

    resolver = LockfileResolver(
        project_path=project,
        user_space=user,
        system_space=str(tmp_path / "system"),
    )
    return resolver, project, lockfiles_dir


class TestLockfileResolvedPath:
    """get_lockfile() attaches _resolved_path so error messages can reference it."""

    def test_get_lockfile_attaches_resolved_path(self, lockfile_env):
        """Loaded lockfile should have _resolved_path pointing to the file on disk."""
        resolver, project, lockfiles_dir = lockfile_env

        lockfile_path = lockfiles_dir / "my/tool@1.0.0.lock.json"
        lockfile_path.parent.mkdir(parents=True, exist_ok=True)
        lockfile_path.write_text(json.dumps({
            "lockfile_version": 1,
            "generated_at": "2026-01-01T00:00:00Z",
            "root": {
                "tool_id": "my/tool",
                "version": "1.0.0",
                "integrity": "abc123",
                "provider_id": "ryeos-core",
                "provider_version": "0.1.0",
            },
            "resolved_chain": [],
        }))

        lockfile = resolver.get_lockfile("my/tool", "1.0.0")
        assert lockfile is not None
        assert hasattr(lockfile, "_resolved_path")
        assert lockfile._resolved_path == lockfile_path

    def test_resolved_path_prefers_project_over_user(self, lockfile_env):
        """Project lockfile should be resolved over user lockfile."""
        resolver, project, lockfiles_dir = lockfile_env

        data = json.dumps({
            "lockfile_version": 1,
            "generated_at": "2026-01-01T00:00:00Z",
            "root": {
                "tool_id": "shared/tool",
                "version": "2.0.0",
                "integrity": "xyz",
                "provider_id": "ryeos-core",
                "provider_version": "0.1.0",
            },
            "resolved_chain": [],
        })

        # Write to both project and user
        proj_path = lockfiles_dir / "shared/tool@2.0.0.lock.json"
        proj_path.parent.mkdir(parents=True, exist_ok=True)
        proj_path.write_text(data)

        user_path = resolver.user_dir / "shared/tool@2.0.0.lock.json"
        user_path.parent.mkdir(parents=True, exist_ok=True)
        user_path.write_text(data)

        lockfile = resolver.get_lockfile("shared/tool", "2.0.0")
        assert lockfile._resolved_path == proj_path

    def test_no_lockfile_returns_none(self, lockfile_env):
        """Missing lockfile should return None, not crash."""
        resolver, _, _ = lockfile_env
        assert resolver.get_lockfile("nonexistent/tool", "1.0.0") is None

    def test_corrupt_lockfile_returns_none(self, lockfile_env):
        """Corrupt lockfile should return None gracefully."""
        resolver, _, lockfiles_dir = lockfile_env

        bad_path = lockfiles_dir / "bad/tool@1.0.0.lock.json"
        bad_path.parent.mkdir(parents=True, exist_ok=True)
        bad_path.write_text("not valid json {{{")

        assert resolver.get_lockfile("bad/tool", "1.0.0") is None
