"""Tests for installed bundle resolution.

Installed bundles merge their items into the top-level .ai/ space
(e.g. ~/.ai/tools/, {project}/.ai/tools/). The bundles/ dir under .ai/
only holds metadata (manifest.yaml, .bundle-lock.json).

Items from installed bundles are discoverable via the normal project/user
space resolution — no special bundle scanning needed.
"""

import os
from pathlib import Path

import pytest

from rye.constants import AI_DIR, ItemType
from rye.utils.resolvers import (
    DirectiveResolver,
    ToolResolver,
    KnowledgeResolver,
)


class TestInstalledBundleResolution:
    """Installed bundle items live in top-level .ai/ and resolve normally."""

    @pytest.fixture
    def space_setup(self, tmp_path, monkeypatch):
        """Set up project + user space with bundle metadata + merged items."""
        project = tmp_path / "project"
        user = tmp_path / "user"

        # Project space: items merged from a bundle
        (project / AI_DIR / "tools").mkdir(parents=True)
        (project / AI_DIR / "tools" / "bundle_tool.py").write_text("print('from bundle')")
        (project / AI_DIR / "directives").mkdir(parents=True)
        (project / AI_DIR / "directives" / "bundle_dir.md").write_text("# from bundle")
        (project / AI_DIR / "knowledge").mkdir(parents=True)
        (project / AI_DIR / "knowledge" / "bundle_ref.md").write_text("# ref")

        # Bundle metadata (not scanned for items)
        bundle_meta = project / AI_DIR / "bundles" / "proj-bundle"
        bundle_meta.mkdir(parents=True)
        (bundle_meta / "manifest.yaml").write_text("bundle_id: proj-bundle\n")
        (bundle_meta / ".bundle-lock.json").write_text('{"files": []}')

        # User space: items merged from a bundle
        (user / AI_DIR / "tools").mkdir(parents=True)
        (user / AI_DIR / "tools" / "user_bundle_tool.py").write_text("print('user')")
        (user / AI_DIR / "directives").mkdir(parents=True)
        (user / AI_DIR / "knowledge").mkdir(parents=True)

        # User bundle metadata
        user_bundle_meta = user / AI_DIR / "bundles" / "user-bundle"
        user_bundle_meta.mkdir(parents=True)
        (user_bundle_meta / "manifest.yaml").write_text("bundle_id: user-bundle\n")

        monkeypatch.setenv("USER_SPACE", str(user))

        return project, user

    def test_tool_from_bundle_found_in_project(self, space_setup):
        """Tool installed by a bundle is found via normal project resolution."""
        project, _ = space_setup
        resolver = ToolResolver(project_path=project)
        found = resolver.resolve("bundle_tool")
        assert found is not None
        assert found.name == "bundle_tool.py"

    def test_tool_from_bundle_found_in_user(self, space_setup):
        """Tool installed by a bundle is found via normal user resolution."""
        project, _ = space_setup
        resolver = ToolResolver(project_path=project)
        found = resolver.resolve("user_bundle_tool")
        assert found is not None
        assert found.name == "user_bundle_tool.py"

    def test_directive_from_bundle_found(self, space_setup):
        project, _ = space_setup
        resolver = DirectiveResolver(project_path=project)
        found = resolver.resolve("bundle_dir")
        assert found is not None

    def test_knowledge_from_bundle_found(self, space_setup):
        project, _ = space_setup
        resolver = KnowledgeResolver(project_path=project)
        found = resolver.resolve("bundle_ref")
        assert found is not None

    def test_bundle_metadata_not_in_search_paths(self, space_setup):
        """bundles/ dir should NOT appear as a search path."""
        project, _ = space_setup
        resolver = ToolResolver(project_path=project)
        paths = resolver.get_search_paths()
        labels = [label for _, label in paths]
        # No "bundle:" prefixed labels
        assert not any("bundle:" in label for label in labels)

    def test_search_paths_are_project_user_system(self, space_setup):
        """Search paths remain project → user → system only."""
        project, _ = space_setup
        resolver = ToolResolver(project_path=project)
        paths = resolver.get_search_paths()
        non_system = [label for _, label in paths if not label.startswith("system:")]
        assert non_system == ["project", "user"]

    def test_project_tool_overrides_user_bundle_tool(self, space_setup):
        """Project space items take priority over user space bundle items."""
        project, user = space_setup
        # Same tool in both project and user space
        (project / AI_DIR / "tools" / "shared.py").write_text("project version")
        (user / AI_DIR / "tools" / "shared.py").write_text("user version")

        resolver = ToolResolver(project_path=project)
        found = resolver.resolve("shared")
        assert found is not None
        assert "project" in str(found)
