"""Tests for kind_from_path — single source of truth for CAS item classification.

Covers:
- All recognised .ai/ subdirectories (directives, tools, knowledge, config)
- Unrecognised paths return None
- Bundler's _classify_file delegates to kind_from_path
- Build and pull produce identical object_hash for config files (the original bug)
"""

import importlib.util

import pytest

from conftest import get_bundle_path
from rye.cas.store import kind_from_path, ingest_item
from rye.constants import AI_DIR, ItemType

# Load bundler module from core bundle (it's a .ai/tools/ file, not a normal package)
_BUNDLER_PATH = get_bundle_path("core", "tools/rye/core/bundler/bundler.py")
_spec = importlib.util.spec_from_file_location("bundler", _BUNDLER_PATH)
_bundler_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_bundler_mod)
_classify_file = _bundler_mod._classify_file


class TestKindFromPath:
    """kind_from_path returns canonical CAS kinds from SIGNABLE_KINDS."""

    @pytest.mark.parametrize("rel_path,expected", [
        (f"{AI_DIR}/directives/rye/email/reply.md", "directive"),
        (f"{AI_DIR}/tools/rye/email/router.py", "tool"),
        (f"{AI_DIR}/tools/rye/email/handle_inbound.yaml", "tool"),
        (f"{AI_DIR}/knowledge/rye/email/email-directives.md", "knowledge"),
        (f"{AI_DIR}/config/email/email.yaml", "config"),
        (f"{AI_DIR}/config/keys/trusted/6ea18199041a1ea8.toml", "config"),
    ])
    def test_recognised_kinds(self, rel_path, expected):
        assert kind_from_path(rel_path) == expected

    @pytest.mark.parametrize("rel_path", [
        f"{AI_DIR}/bundles/ryeos-email/manifest.yaml",
        f"{AI_DIR}/objects/ab/cdef1234",
        f"{AI_DIR}/agent/state.json",
        f"{AI_DIR}/cache/something.tmp",
        f"{AI_DIR}/lockfiles/head.json",
        "src/main.py",
        "README.md",
    ])
    def test_unrecognised_returns_none(self, rel_path):
        assert kind_from_path(rel_path) is None

    def test_covers_all_signable_kinds(self):
        """Every entry in SIGNABLE_KINDS is recognised by kind_from_path."""
        for kind, dir_name in ItemType.SIGNABLE_KINDS.items():
            rel_path = f"{AI_DIR}/{dir_name}/test_file.txt"
            assert kind_from_path(rel_path) == kind


class TestBundlerClassifyDelegates:
    """Bundler's _classify_file delegates to kind_from_path."""

    def test_config_file_classified_as_config(self):
        assert _classify_file(f"{AI_DIR}/config/keys/trusted/abc.toml") == "config"
        assert _classify_file(f"{AI_DIR}/config/email/email.yaml") == "config"

    def test_standard_kinds(self):
        assert _classify_file(f"{AI_DIR}/tools/rye/email/router.py") == "tool"
        assert _classify_file(f"{AI_DIR}/directives/rye/email/reply.md") == "directive"
        assert _classify_file(f"{AI_DIR}/knowledge/rye/email/info.md") == "knowledge"

    def test_unknown_falls_back_to_asset(self):
        assert _classify_file(f"{AI_DIR}/bundles/x/manifest.yaml") == "asset"
        assert _classify_file("some/random/file.txt") == "asset"

    def test_agrees_with_kind_from_path(self):
        """For every SIGNABLE_KINDS entry, _classify_file and kind_from_path agree."""
        for kind, dir_name in ItemType.SIGNABLE_KINDS.items():
            rel_path = f"{AI_DIR}/{dir_name}/some/nested/file.py"
            assert _classify_file(rel_path) == kind_from_path(rel_path) == kind


class TestObjectHashConsistency:
    """Build and pull must produce identical object_hash for the same file.

    This is the original bug: bundler used "asset" for config files,
    pull used "tool" (fallback). Different kind in ItemSource
    → different object_hash.
    """

    def _write_file(self, project_path, rel_parts, content):
        """Create a file under project_path and return (file_path, rel_path)."""
        file_path = project_path
        for part in rel_parts:
            file_path = file_path / part
        file_path.parent.mkdir(parents=True, exist_ok=True)
        file_path.write_text(content)
        rel_path = str(file_path.relative_to(project_path))
        return file_path, rel_path

    def test_different_kind_produces_different_hash(self, tmp_path):
        """Proves why consistent classification matters — wrong kind = wrong hash."""
        file_path, _ = self._write_file(
            tmp_path,
            [AI_DIR, "config", "keys", "trusted", "test.toml"],
            'fingerprint = "abc"\n',
        )

        ref_config = ingest_item("config", file_path, tmp_path)
        ref_tool = ingest_item("tool", file_path, tmp_path)

        assert ref_config.object_hash != ref_tool.object_hash

    def test_build_and_pull_agree_on_config_hash(self, tmp_path):
        """Build-side (_classify_file → ingest_item) and pull-side
        (kind_from_path → ingest_item) produce the same object_hash
        for a config file. Regression test for the hash mismatch bug."""
        file_path, rel_path = self._write_file(
            tmp_path,
            [AI_DIR, "config", "keys", "trusted", "test.toml"],
            'fingerprint = "abc"\n',
        )

        # Build side
        build_kind = _classify_file(rel_path)
        build_ref = ingest_item(build_kind, file_path, tmp_path)

        # Pull side
        pull_kind = kind_from_path(rel_path)
        pull_ref = ingest_item(pull_kind, file_path, tmp_path)

        assert build_kind == pull_kind == "config"
        assert build_ref.object_hash == pull_ref.object_hash

    def test_build_and_pull_agree_on_tool_hash(self, tmp_path):
        """Same consistency check for a standard tool file."""
        file_path, rel_path = self._write_file(
            tmp_path,
            [AI_DIR, "tools", "rye", "email", "router.py"],
            "def route(): pass\n",
        )

        build_kind = _classify_file(rel_path)
        build_ref = ingest_item(build_kind, file_path, tmp_path)

        pull_kind = kind_from_path(rel_path)
        pull_ref = ingest_item(pull_kind, file_path, tmp_path)

        assert build_kind == pull_kind == "tool"
        assert build_ref.object_hash == pull_ref.object_hash

    def test_build_and_pull_agree_on_directive_hash(self, tmp_path):
        """Same consistency check for a directive file."""
        file_path, rel_path = self._write_file(
            tmp_path,
            [AI_DIR, "directives", "rye", "email", "reply.md"],
            "# Reply\nDraft a reply.\n",
        )

        build_kind = _classify_file(rel_path)
        build_ref = ingest_item(build_kind, file_path, tmp_path)

        pull_kind = kind_from_path(rel_path)
        pull_ref = ingest_item(pull_kind, file_path, tmp_path)

        assert build_kind == pull_kind == "directive"
        assert build_ref.object_hash == pull_ref.object_hash
