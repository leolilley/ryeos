"""Tests for temp materializer."""

import pytest

from rye.primitives import cas
from rye.cas.manifest import build_manifest
from rye.cas.materializer import (
    ExecutionPaths,
    materialize,
    cleanup,
    get_system_version,
)
from rye.cas.store import cas_root
from rye.constants import AI_DIR


def _make_project(root, files=None):
    if files is None:
        files = {
            ".ai/tools/my_tool.py": "print('hello')\n",
            ".ai/directives/my_dir.md": "# directive\ncontent here\n",
            ".ai/config/agent/agent.yaml": "model: gpt-4\n",
        }
    for rel_path, content in files.items():
        full = root / rel_path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)


def _make_user(root):
    files = {
        ".ai/config/agent/agent.yaml": "model: gpt-4-turbo\n",
        ".ai/config/keys/signing/public_key.pem": "PEM DATA\n",
    }
    for rel_path, content in files.items():
        full = root / rel_path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)


class TestMaterialize:
    def test_round_trip_project(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project)

        # Build manifest using project as CAS root
        ph, _ = build_manifest(project, "project")

        # Build empty user manifest
        user = tmp_path / "user"
        user.mkdir()
        _make_user(user)
        uh, _ = build_manifest(user, "user", project_path=project)

        # Materialize
        paths = materialize(ph, uh, cas_root(project), tmp_base=tmp_path)
        try:
            # Project files materialized
            assert (paths.project_path / ".ai/tools/my_tool.py").exists()
            assert (
                paths.project_path / ".ai/tools/my_tool.py"
            ).read_text() == "print('hello')\n"
            assert (paths.project_path / ".ai/directives/my_dir.md").exists()
            assert (paths.project_path / ".ai/config/agent/agent.yaml").exists()

            # User files materialized
            assert (paths.user_space / ".ai/config/agent/agent.yaml").exists()
            assert (
                paths.user_space / ".ai/config/agent/agent.yaml"
            ).read_text() == "model: gpt-4-turbo\n"
            assert (
                paths.user_space / ".ai/config/keys/signing/public_key.pem"
            ).exists()

            # CAS root points to shared CAS
            assert paths.cas_root == cas_root(project)
        finally:
            cleanup(paths)

    def test_round_trip_with_project_files(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project, {
            ".ai/tools/x.py": "tool\n",
            ".ai/config/cas/manifest.yaml": (
                "sync:\n"
                "  include:\n"
                "    - src/\n"
            ),
        })
        (project / "src" / "app.py").parent.mkdir(parents=True)
        (project / "src" / "app.py").write_text("code\n")

        ph, pm = build_manifest(project, "project")
        assert "src/app.py" in pm["files"]

        # Empty user
        user = tmp_path / "user"
        user.mkdir()
        (user / AI_DIR).mkdir()
        uh, _ = build_manifest(user, "user", project_path=project)

        paths = materialize(ph, uh, cas_root(project), tmp_base=tmp_path)
        try:
            assert (paths.project_path / "src/app.py").read_text() == "code\n"
            assert (paths.project_path / ".ai/tools/x.py").read_text() == "tool\n"
        finally:
            cleanup(paths)

    def test_missing_manifest_raises(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        with pytest.raises(FileNotFoundError, match="Manifest"):
            materialize("0" * 64, "0" * 64, root, tmp_base=tmp_path)

    def test_cleanup_removes_dirs(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project)
        ph, _ = build_manifest(project, "project")

        user = tmp_path / "user"
        user.mkdir()
        (user / AI_DIR).mkdir()
        uh, _ = build_manifest(user, "user", project_path=project)

        paths = materialize(ph, uh, cas_root(project), tmp_base=tmp_path)
        base = paths._base
        assert base.exists()
        cleanup(paths)
        assert not base.exists()

    def test_cleanup_idempotent(self, tmp_path):
        paths = ExecutionPaths(
            project_path=tmp_path / "gone" / "project",
            user_space=tmp_path / "gone" / "user",
            cas_root=tmp_path,
            _base=tmp_path / "gone",
        )
        # Should not raise even if dirs don't exist
        cleanup(paths)

    def test_preserves_directory_structure(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        _make_project(project, {
            ".ai/tools/rye/core/extractors/yaml_ext.py": "# extractor\n",
            ".ai/directives/workflows/deploy.md": "# deploy\n",
        })

        ph, _ = build_manifest(project, "project")
        user = tmp_path / "user"
        user.mkdir()
        (user / AI_DIR).mkdir()
        uh, _ = build_manifest(user, "user", project_path=project)

        paths = materialize(ph, uh, cas_root(project), tmp_base=tmp_path)
        try:
            assert (
                paths.project_path / ".ai/tools/rye/core/extractors/yaml_ext.py"
            ).exists()
            assert (
                paths.project_path / ".ai/directives/workflows/deploy.md"
            ).exists()
        finally:
            cleanup(paths)


class TestGetSystemVersion:
    def test_returns_string(self):
        v = get_system_version()
        assert isinstance(v, str)
        assert v != ""
