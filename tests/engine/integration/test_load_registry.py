"""Tests for registry load integration in FetchTool (ID mode) via RegistrySpaceProvider."""

import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from rye.actions._resolve import resolve_item


@pytest.fixture
def temp_project():
    """Create minimal project directory."""
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        (project_root / ".ai" / "directives").mkdir(parents=True)
        (project_root / ".ai" / "tools").mkdir(parents=True)
        yield project_root


def _mock_registry_provider(pull_result=None):
    """Create a mock RegistrySpaceProvider for registry."""
    provider = MagicMock()
    provider.provider_id = "registry"
    if pull_result is None:
        pull_result = {
            "status": "success",
            "content": "# Bootstrapper\n\nSets up a new project.",
            "item_type": "directive",
            "item_id": "acme/core/bootstrap",
            "version": "1.0.0",
            "source": "registry",
            "metadata": {
                "author": "acme",
                "namespace": "acme",
                "category": "core",
                "name": "bootstrap",
                "signature": {},
            },
        }
    provider.pull = AsyncMock(return_value=pull_result)
    return provider


@pytest.mark.asyncio
class TestLoadFromRegistry:
    """Test source=registry loads items via RegistrySpaceProvider."""

    async def test_load_from_registry(self, temp_project):
        """source=registry pulls content via provider."""
        provider = _mock_registry_provider()

        with patch(
            "rye.actions._resolve.get_registry_provider",
            return_value=provider,
        ):
            result = await resolve_item(
                str(temp_project),
                item_ref="directive:acme/core/bootstrap",
                project_path=str(temp_project),
                source="registry",
            )

        assert result["status"] == "success"
        assert "# Bootstrapper" in result["content"]
        assert result["source"] == "registry"
        assert result["item_ref"] == "directive:acme/core/bootstrap"
        provider.pull.assert_called_once_with(
            kind="directive",
            bare_id="acme/core/bootstrap",
            version=None,
        )

    async def test_load_from_registry_with_version(self, temp_project):
        """source=registry passes version to provider."""
        provider = _mock_registry_provider()

        with patch(
            "rye.actions._resolve.get_registry_provider",
            return_value=provider,
        ):
            result = await resolve_item(
                str(temp_project),
                item_ref="directive:acme/core/bootstrap",
                project_path=str(temp_project),
                source="registry",
                version="2.0.0",
            )

        assert result["status"] == "success"
        provider.pull.assert_called_once_with(
            kind="directive",
            bare_id="acme/core/bootstrap",
            version="2.0.0",
        )

    async def test_load_from_registry_copies_to_project(self, temp_project):
        """source=registry with destination=project writes content to disk."""
        provider = _mock_registry_provider()

        with patch(
            "rye.actions._resolve.get_registry_provider",
            return_value=provider,
        ):
            result = await resolve_item(
                str(temp_project),
                item_ref="directive:acme/core/bootstrap",
                project_path=str(temp_project),
                source="registry",
                destination="project",
            )

        assert result["status"] == "success"
        assert result["copied_to"] == "project"
        dest = Path(result["destination_path"])
        assert dest.exists()
        assert "# Bootstrapper" in dest.read_text()
        # Should be under .ai/directives/core/bootstrap.md
        assert "core/bootstrap.md" in str(dest)

    async def test_load_from_registry_copies_to_user(self, temp_project):
        """source=registry with destination=user writes content to user space."""
        provider = _mock_registry_provider()

        with patch(
            "rye.actions._resolve.get_registry_provider",
            return_value=provider,
        ):
            result = await resolve_item(
                str(temp_project),
                item_ref="directive:acme/core/bootstrap",
                project_path=str(temp_project),
                source="registry",
                destination="user",
            )

        assert result["status"] == "success"
        assert result["copied_to"] == "user"
        assert "destination_path" in result

    async def test_load_from_registry_error(self, temp_project):
        """Registry pull errors are returned cleanly."""
        provider = _mock_registry_provider(
            pull_result={"error": "Item not found"}
        )

        with patch(
            "rye.actions._resolve.get_registry_provider",
            return_value=provider,
        ):
            result = await resolve_item(
                str(temp_project),
                item_ref="directive:acme/core/nonexistent",
                project_path=str(temp_project),
                source="registry",
            )

        assert result["status"] == "error"
        assert "Item not found" in result["error"]

    async def test_load_from_registry_no_provider(self, temp_project):
        """Missing provider returns clean error."""
        with patch(
            "rye.actions._resolve.get_registry_provider",
            return_value=None,
        ):
            result = await resolve_item(
                str(temp_project),
                item_ref="directive:acme/core/bootstrap",
                project_path=str(temp_project),
                source="registry",
            )

        assert result["status"] == "error"
        assert "not found" in result["error"].lower()

    async def test_load_tool_for_registry(self, temp_project):
        """source=registry works for tool item type."""
        provider = _mock_registry_provider({
            "status": "success",
            "content": "__version__ = '1.0.0'\ndef run(): pass\n",
            "item_type": "tool",
            "item_id": "acme/utils/helper",
            "version": "1.0.0",
            "source": "registry",
            "metadata": {
                "author": "acme",
                "namespace": "acme",
                "category": "utils",
                "name": "helper",
                "signature": {},
            },
        })

        with patch(
            "rye.actions._resolve.get_registry_provider",
            return_value=provider,
        ):
            result = await resolve_item(
                str(temp_project),
                item_ref="tool:acme/utils/helper",
                project_path=str(temp_project),
                source="registry",
                destination="project",
            )

        assert result["status"] == "success"
        dest = Path(result["destination_path"])
        assert dest.suffix == ".py"
        assert dest.exists()
