"""Tests for source manifest builder."""

import os
import re
import pytest

from rye.primitives import cas
from rye.cas.manifest import (
    build_manifest,
    _is_hard_excluded,
    _load_manifest_policy,
    _get_hard_exclude_names,
    _get_hard_exclude_patterns,
    _walk_ai_items,
    _walk_project_files,
)
from rye.cas.store import cas_root
from rye.constants import AI_DIR


# Default policy values matching the core bundle config
_DEFAULT_EXCLUDE_NAMES = {"private_key.pem"}
_DEFAULT_EXCLUDE_PATTERNS = [
    re.compile(r"^\.env$"),
    re.compile(r"^\.env\."),
    re.compile(r".*\.secrets?$"),
]


def _make_project(tmp_path, files=None):
    """Create a minimal project with .ai/ files."""
    if files is None:
        files = {
            ".ai/tools/my_tool.py": "print('hello')\n",
            ".ai/directives/my_dir.md": "# directive\n",
            ".ai/knowledge/info.md": "# knowledge\n",
            ".ai/config/agent/agent.yaml": "model: gpt-4\n",
        }
    for rel_path, content in files.items():
        full = tmp_path / rel_path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)
    return tmp_path


class TestHardExclusions:
    def test_private_key(self):
        assert (
            _is_hard_excluded(
                "private_key.pem", _DEFAULT_EXCLUDE_NAMES, _DEFAULT_EXCLUDE_PATTERNS
            )
            is True
        )

    def test_env(self):
        assert (
            _is_hard_excluded(".env", _DEFAULT_EXCLUDE_NAMES, _DEFAULT_EXCLUDE_PATTERNS)
            is True
        )

    def test_env_local(self):
        assert (
            _is_hard_excluded(
                ".env.local", _DEFAULT_EXCLUDE_NAMES, _DEFAULT_EXCLUDE_PATTERNS
            )
            is True
        )

    def test_env_production(self):
        assert (
            _is_hard_excluded(
                ".env.production", _DEFAULT_EXCLUDE_NAMES, _DEFAULT_EXCLUDE_PATTERNS
            )
            is True
        )

    def test_secrets_file(self):
        assert (
            _is_hard_excluded(
                "db.secret", _DEFAULT_EXCLUDE_NAMES, _DEFAULT_EXCLUDE_PATTERNS
            )
            is True
        )
        assert (
            _is_hard_excluded(
                "api.secrets", _DEFAULT_EXCLUDE_NAMES, _DEFAULT_EXCLUDE_PATTERNS
            )
            is True
        )

    def test_normal_files_pass(self):
        assert (
            _is_hard_excluded(
                "tool.py", _DEFAULT_EXCLUDE_NAMES, _DEFAULT_EXCLUDE_PATTERNS
            )
            is False
        )
        assert (
            _is_hard_excluded(
                "agent.yaml", _DEFAULT_EXCLUDE_NAMES, _DEFAULT_EXCLUDE_PATTERNS
            )
            is False
        )
        assert (
            _is_hard_excluded(
                "public_key.pem", _DEFAULT_EXCLUDE_NAMES, _DEFAULT_EXCLUDE_PATTERNS
            )
            is False
        )


class TestWalkAiItems:
    def test_ingests_all_ai_files(self, tmp_path):
        _make_project(tmp_path)
        items = _walk_ai_items(tmp_path, tmp_path)
        assert ".ai/tools/my_tool.py" in items
        assert ".ai/directives/my_dir.md" in items
        assert ".ai/knowledge/info.md" in items
        assert ".ai/config/agent/agent.yaml" in items

    def test_skips_objects_dir(self, tmp_path):
        _make_project(
            tmp_path,
            {
                ".ai/tools/x.py": "x\n",
                ".ai/state/objects/blobs/ab/cd/deadbeef": "should skip",
            },
        )
        items = _walk_ai_items(tmp_path, tmp_path)
        assert not any("state/objects" in k for k in items)

    def test_skips_state_dir(self, tmp_path):
        _make_project(
            tmp_path,
            {
                ".ai/tools/x.py": "x\n",
                ".ai/state/threads/t1/state.json": "should skip",
            },
        )
        items = _walk_ai_items(tmp_path, tmp_path)
        assert not any("state" in k for k in items)

    def test_skips_hard_excluded(self, tmp_path):
        _make_project(
            tmp_path,
            {
                ".ai/tools/x.py": "x\n",
                ".ai/config/keys/signing/private_key.pem": "SECRET",
                ".ai/.env": "API_KEY=abc",
                ".ai/db.secret": "password",
            },
        )
        items = _walk_ai_items(tmp_path, tmp_path)
        assert ".ai/tools/x.py" in items
        assert not any("private_key" in k for k in items)
        assert not any(".env" in k for k in items)
        assert not any("secret" in k for k in items)

    def test_empty_ai_dir(self, tmp_path):
        (tmp_path / AI_DIR).mkdir()
        items = _walk_ai_items(tmp_path, tmp_path)
        assert items == {}

    def test_no_ai_dir(self, tmp_path):
        items = _walk_ai_items(tmp_path, tmp_path)
        assert items == {}


class TestWalkProjectFiles:
    def test_includes_matched_files(self, tmp_path):
        _make_project(
            tmp_path,
            {
                ".ai/tools/x.py": "x\n",
            },
        )
        (tmp_path / "src" / "main.py").parent.mkdir(parents=True)
        (tmp_path / "src" / "main.py").write_text("print(1)\n")

        files = _walk_project_files(
            tmp_path,
            tmp_path,
            include=["src/"],
            exclude=[],
        )
        assert "src/main.py" in files

    def test_excludes_matched_files(self, tmp_path):
        (tmp_path / "src" / "main.py").parent.mkdir(parents=True)
        (tmp_path / "src" / "main.py").write_text("ok\n")
        (tmp_path / "node_modules" / "pkg.js").parent.mkdir(parents=True)
        (tmp_path / "node_modules" / "pkg.js").write_text("skip\n")

        files = _walk_project_files(
            tmp_path,
            tmp_path,
            include=["src/", "node_modules/"],
            exclude=["node_modules/"],
        )
        assert "src/main.py" in files
        assert "node_modules/pkg.js" not in files

    def test_ai_dir_always_excluded_from_files(self, tmp_path):
        _make_project(
            tmp_path,
            {
                ".ai/tools/x.py": "x\n",
            },
        )
        files = _walk_project_files(
            tmp_path,
            tmp_path,
            include=[".ai/"],
            exclude=[],
        )
        # .ai/ include is filtered out — it goes in items, not files
        assert files == {}

    def test_no_includes_returns_empty(self, tmp_path):
        (tmp_path / "src" / "main.py").parent.mkdir(parents=True)
        (tmp_path / "src" / "main.py").write_text("ok\n")
        files = _walk_project_files(tmp_path, tmp_path, include=[], exclude=[])
        assert files == {}

    def test_hard_excluded_files_skipped(self, tmp_path):
        (tmp_path / "src").mkdir()
        (tmp_path / "src" / "app.py").write_text("ok\n")
        (tmp_path / "src" / ".env").write_text("SECRET=x\n")
        (tmp_path / "src" / "db.secret").write_text("pw\n")

        files = _walk_project_files(
            tmp_path,
            tmp_path,
            include=["src/"],
            exclude=[],
        )
        assert "src/app.py" in files
        assert "src/.env" not in files
        assert "src/db.secret" not in files


class TestBuildManifest:
    def test_project_manifest_ai_only(self, tmp_path):
        _make_project(tmp_path)
        h, m = build_manifest(tmp_path, "project")

        assert m["schema"] == 2
        assert m["kind"] == "source_manifest"
        assert m["space"] == "project"
        assert len(m["items"]) == 4
        assert m["files"] == {}
        assert len(h) == 64

    def test_project_manifest_with_remote_config(self, tmp_path):
        _make_project(
            tmp_path,
            {
                ".ai/tools/x.py": "x\n",
                ".ai/config/cas/manifest.yaml": (
                    "sync:\n  include:\n    - src/\n  exclude:\n    - node_modules/\n"
                ),
            },
        )
        (tmp_path / "src" / "app.py").parent.mkdir(parents=True)
        (tmp_path / "src" / "app.py").write_text("code\n")
        (tmp_path / "node_modules" / "pkg.js").parent.mkdir(parents=True)
        (tmp_path / "node_modules" / "pkg.js").write_text("skip\n")

        h, m = build_manifest(tmp_path, "project")
        assert "src/app.py" in m["files"]
        assert "node_modules/pkg.js" not in m["files"]
        # .ai/ items still present
        assert ".ai/tools/x.py" in m["items"]

    def test_user_manifest_no_files(self, tmp_path):
        _make_project(
            tmp_path,
            {
                ".ai/config/agent/agent.yaml": "model: gpt-4\n",
                ".ai/config/keys/signing/public_key.pem": "PEM DATA\n",
            },
        )
        h, m = build_manifest(tmp_path, "user")
        assert m["space"] == "user"
        assert m["files"] == {}
        assert len(m["items"]) == 2

    def test_manifest_stored_in_cas(self, tmp_path):
        _make_project(tmp_path)
        h, m = build_manifest(tmp_path, "project")
        # Should be retrievable from CAS
        retrieved = cas.get_object(h, cas_root(tmp_path))
        assert retrieved == m

    def test_deterministic(self, tmp_path):
        _make_project(tmp_path)
        h1, _ = build_manifest(tmp_path, "project")
        h2, _ = build_manifest(tmp_path, "project")
        assert h1 == h2
