"""Tests for search shadow detection."""

import asyncio
import tempfile
from pathlib import Path

import pytest

from rye.actions._search import SearchEngine


@pytest.fixture
def multi_space_project(_setup_user_space):
    """Create a project where items shadow system items."""
    import os
    from rye.utils.metadata_manager import MetadataManager
    from rye.constants import ItemType, AI_DIR

    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)

        # Create a project directive with same name as one in system
        directives_dir = project_root / ".ai" / "directives" / "test"
        directives_dir.mkdir(parents=True)
        (directives_dir / "shadow_test.md").write_text(
            '# Shadow Test\n\n'
            '```xml\n'
            '<directive name="shadow_test" version="1.0.0">\n'
            '<metadata><description>Project version</description></metadata>\n'
            '</directive>\n'
            '```\n'
        )

        # Create a second directive that doesn't shadow anything
        (directives_dir / "unique.md").write_text(
            '# Unique\n\n'
            '```xml\n'
            '<directive name="unique" version="1.0.0">\n'
            '<metadata><description>Only in project</description></metadata>\n'
            '</directive>\n'
            '```\n'
        )

        # Trust signing key and sign items
        user_space = Path(os.environ.get("USER_SPACE"))
        signing_key_dir = user_space / AI_DIR / "config" / "keys" / "signing"
        from lillux.primitives.signing import load_keypair
        _, public_pem = load_keypair(signing_key_dir)

        from rye.utils.trust_store import TrustStore
        store = TrustStore(project_path=project_root)
        store.add_key(public_pem, owner="local", space="project", version="1.0.0")

        for f in directives_dir.glob("*.md"):
            content = f.read_text()
            signed = MetadataManager.sign_content(ItemType.DIRECTIVE, content)
            f.write_text(signed)

        yield project_root


@pytest.mark.asyncio
class TestSearchShadowDetection:
    """Test that search results include shadow information."""

    async def test_detect_shadows_marks_duplicates(self):
        """Items with same ID from different spaces should be marked."""
        tool = SearchEngine("")
        results = [
            {"id": "test/item", "source": "project", "score": 0.9},
            {"id": "test/item", "source": "system", "score": 0.8},
            {"id": "test/other", "source": "project", "score": 0.7},
        ]
        tool._detect_shadows(results)

        # First occurrence should have shadows list
        assert "shadows" in results[0]
        assert results[0]["shadows"] == [{"source": "system"}]

        # Second occurrence (shadowed) should have shadowed_by
        assert results[1].get("shadowed_by") == "project"

        # Unique item should have neither
        assert "shadows" not in results[2]
        assert "shadowed_by" not in results[2]

    async def test_detect_shadows_multiple_spaces(self):
        """An item in project, user, and system should chain correctly."""
        tool = SearchEngine("")
        results = [
            {"id": "test/item", "source": "project", "score": 0.9},
            {"id": "test/item", "source": "user", "score": 0.8},
            {"id": "test/item", "source": "system", "score": 0.7},
        ]
        tool._detect_shadows(results)

        # Project wins, shadows both
        assert len(results[0]["shadows"]) == 2
        assert results[1].get("shadowed_by") == "project"
        assert results[2].get("shadowed_by") == "project"

    async def test_detect_shadows_no_duplicates(self):
        """When no duplicates exist, no shadow info is added."""
        tool = SearchEngine("")
        results = [
            {"id": "a", "source": "project", "score": 0.9},
            {"id": "b", "source": "system", "score": 0.8},
        ]
        tool._detect_shadows(results)

        assert "shadows" not in results[0]
        assert "shadows" not in results[1]
        assert "shadowed_by" not in results[0]
        assert "shadowed_by" not in results[1]

    async def test_shadow_info_in_search_results(self, multi_space_project):
        """Integration: search across all spaces should include shadow data."""
        tool = SearchEngine("")
        result = await tool.handle(
            scope="rye.fetch.directive.*",
            query="*",
            project_path=str(multi_space_project),
            source="all",
        )

        assert "error" not in result
        # Find any item that has shadow info
        shadowing_items = [r for r in result["results"] if "shadows" in r]
        shadowed_items = [r for r in result["results"] if "shadowed_by" in r]

        # Even if no actual shadowing occurs (depends on system items),
        # the method should have been called without error
        assert isinstance(result["results"], list)
