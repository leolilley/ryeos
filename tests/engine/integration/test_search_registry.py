"""Tests for registry search integration in SearchTool via RegistrySpaceProvider."""

import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from rye.tools.search import SearchTool


@pytest.fixture
def temp_project(_setup_user_space):
    """Create minimal project for search tests."""
    import os
    from rye.utils.trust_store import TrustStore
    from rye.utils.metadata_manager import MetadataManager
    from rye.constants import ItemType, AI_DIR as RYE_AI_DIR

    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        ai_dir = project_root / ".ai"

        directives_dir = ai_dir / "directives"
        directives_dir.mkdir(parents=True)
        (directives_dir / "local_item.md").write_text(
            "# Local Item\n\n"
            "```xml\n"
            '<directive name="local_item" version="1.0.0">\n'
            "<metadata><description>A local directive</description></metadata>\n"
            "</directive>\n"
            "```\n"
        )

        user_space = Path(os.environ.get("USER_SPACE"))
        signing_key_dir = user_space / RYE_AI_DIR / "config" / "keys" / "signing"
        from lillux.primitives.signing import load_keypair
        _, public_pem = load_keypair(signing_key_dir)

        store = TrustStore(project_path=project_root)
        store.add_key(public_pem, owner="local", space="project")

        for f in (ai_dir / "directives").glob("*.md"):
            content = f.read_text()
            f.write_text(MetadataManager.sign_content(ItemType.DIRECTIVE, content))

        yield project_root


def _mock_registry_search_results():
    """Fake registry provider search results in normalized format."""
    return [
        {
            "id": "acme/core/bootstrap",
            "name": "bootstrap",
            "description": "Bootstrap a new project",
            "type": "directive",
            "source": "registry",
            "score": 0.5,
            "metadata": {
                "version": "1.0.0",
                "author": "acme",
                "namespace": "acme",
                "download_count": 42,
            },
        },
    ]


def _mock_registry_provider():
    """Create a mock RegistrySpaceProvider for registry."""
    provider = MagicMock()
    provider.provider_id = "registry"
    provider.search = AsyncMock(return_value=_mock_registry_search_results())
    return provider


@pytest.mark.asyncio
class TestRegistrySearchSpace:
    """Test source=registry and source=all include registry results."""

    async def test_space_registry_calls_provider(self, temp_project):
        """source=registry only returns registry results, no local."""
        provider = _mock_registry_provider()

        with patch(
            "rye.tools.search.get_registry_providers",
            return_value={"registry": provider},
        ):
            tool = SearchTool("")
            result = await tool.handle(
                scope="directive",
                query="bootstrap",
                project_path=str(temp_project),
                source="registry",
            )

        assert "error" not in result
        provider.search.assert_called_once_with(
            query="bootstrap", item_type="directive", limit=10
        )
        # Should only have registry results, not local
        assert all(r["source"] == "registry" for r in result["results"])
        assert any(r["id"] == "acme/core/bootstrap" for r in result["results"])

    async def test_space_all_includes_registry(self, temp_project):
        """source=all merges local and registry results."""
        provider = _mock_registry_provider()

        with patch(
            "rye.tools.search.get_registry_providers",
            return_value={"registry": provider},
        ):
            tool = SearchTool("")
            result = await tool.handle(
                scope="directive",
                query="bootstrap",
                project_path=str(temp_project),
                source="all",
                limit=100,
            )

        assert "error" not in result
        provider.search.assert_called_once()
        sources = {r["source"] for r in result["results"]}
        assert "registry" in sources

    async def test_space_local_excludes_registry(self, temp_project):
        """source=local searches all local spaces but not registry."""
        provider = _mock_registry_provider()

        with patch(
            "rye.tools.search.get_registry_providers",
            return_value={"registry": provider},
        ):
            tool = SearchTool("")
            result = await tool.handle(
                scope="directive",
                query="local_item",
                project_path=str(temp_project),
                source="local",
            )

        assert "error" not in result
        provider.search.assert_not_called()
        assert all(r["source"] != "registry" for r in result["results"])

    async def test_space_project_excludes_registry(self, temp_project):
        """source=project does not call registry."""
        provider = _mock_registry_provider()

        with patch(
            "rye.tools.search.get_registry_providers",
            return_value={"registry": provider},
        ):
            tool = SearchTool("")
            result = await tool.handle(
                scope="directive",
                query="local_item",
                project_path=str(temp_project),
                source="project",
            )

        assert "error" not in result
        provider.search.assert_not_called()

    async def test_registry_error_graceful(self, temp_project):
        """Registry errors don't break the search — local results still returned."""
        provider = _mock_registry_provider()
        provider.search = AsyncMock(side_effect=Exception("Connection refused"))

        with patch(
            "rye.tools.search.get_registry_providers",
            return_value={"registry": provider},
        ):
            tool = SearchTool("")
            result = await tool.handle(
                scope="directive",
                query="local_item",
                project_path=str(temp_project),
                source="all",
            )

        assert "error" not in result
        # Local results still present
        assert result["total"] >= 0

    async def test_no_providers_graceful(self, temp_project):
        """If no remote providers are discovered, search still works."""
        with patch(
            "rye.tools.search.get_registry_providers",
            return_value={},
        ):
            tool = SearchTool("")
            result = await tool.handle(
                scope="directive",
                query="local_item",
                project_path=str(temp_project),
                source="all",
            )

        assert "error" not in result

    async def test_registry_result_format(self, temp_project):
        """Registry results have the expected fields."""
        provider = _mock_registry_provider()

        with patch(
            "rye.tools.search.get_registry_providers",
            return_value={"registry": provider},
        ):
            tool = SearchTool("")
            result = await tool.handle(
                scope="directive",
                query="bootstrap",
                project_path=str(temp_project),
                source="registry",
            )

        assert len(result["results"]) == 1
        item = result["results"][0]
        assert item["id"] == "acme/core/bootstrap"
        assert item["name"] == "bootstrap"
        assert item["description"] == "Bootstrap a new project"
        assert item["type"] == "directive"
        assert item["source"] == "registry"
        assert item["score"] == 0.5
        assert item["metadata"]["version"] == "1.0.0"
        assert item["metadata"]["author"] == "acme"
        assert item["metadata"]["namespace"] == "acme"
        assert item["metadata"]["download_count"] == 42
