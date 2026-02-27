"""Tests for search tool."""

import asyncio
import tempfile
from pathlib import Path

import pytest

from rye.tools.search import SearchTool


@pytest.fixture
def temp_project(_setup_user_space):
    """Create temporary project with test items."""
    import os
    from rye.utils.trust_store import TrustStore
    from rye.utils.metadata_manager import MetadataManager
    from rye.constants import ItemType, AI_DIR as RYE_AI_DIR
    
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        ai_dir = project_root / ".ai"

        directives_dir = ai_dir / "directives"
        directives_dir.mkdir(parents=True)
        (directives_dir / "create_tool.md").write_text(
            '# Create Tool\n\n'
            '```xml\n'
            '<directive name="create_tool" version="1.0.0">\n'
            '<metadata><description>Create a new tool</description></metadata>\n'
            '</directive>\n'
            '```\n'
        )
        (directives_dir / "bootstrap.md").write_text(
            '# Bootstrap\n\n'
            '```xml\n'
            '<directive name="bootstrap" version="1.0.0">\n'
            '<metadata><description>Bootstrap project</description></metadata>\n'
            '</directive>\n'
            '```\n'
        )

        tools_dir = ai_dir / "tools"
        tools_dir.mkdir(parents=True)
        (tools_dir / "scraper.py").write_text('# Tool scraper\n' "# version=1.0.0")

        knowledge_dir = ai_dir / "knowledge"
        knowledge_dir.mkdir(parents=True)
        (knowledge_dir / "api_patterns.md").write_text(
            "---\ntitle: API Design Patterns\n---\n\nAPI patterns content"
        )

        # Get the signing public key from the setup fixture
        user_space = Path(os.environ.get("USER_SPACE"))
        signing_key_dir = user_space / RYE_AI_DIR / "config" / "keys" / "signing"
        from lillux.primitives.signing import load_keypair
        _, public_pem_signing = load_keypair(signing_key_dir)
        
        # Trust the signing key in this project
        store = TrustStore(project_path=project_root)
        store.add_key(public_pem_signing, owner="local", space="project")

        # Sign items
        for directive_file in (ai_dir / "directives").glob("*.md"):
            content = directive_file.read_text()
            signed = MetadataManager.sign_content(ItemType.DIRECTIVE, content)
            directive_file.write_text(signed)

        for tool_file in (ai_dir / "tools").rglob("*.py"):
            content = tool_file.read_text()
            signed = MetadataManager.sign_content(
                ItemType.TOOL, content, file_path=tool_file, project_path=project_root
            )
            tool_file.write_text(signed)

        for knowledge_file in (ai_dir / "knowledge").glob("*.md"):
            content = knowledge_file.read_text()
            signed = MetadataManager.sign_content(ItemType.KNOWLEDGE, content)
            knowledge_file.write_text(signed)

        yield project_root


@pytest.mark.asyncio
class TestSearchTool:
    """Test search tool."""

    async def test_search_directives(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query="create",
            project_path=str(temp_project),
            space="project",
        )

        assert "error" not in result
        assert result["total"] >= 1
        assert any(r["name"] == "create_tool" for r in result["results"])

    async def test_search_tools(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.tool.*",
            query="scraper",
            project_path=str(temp_project),
            space="project",
        )

        assert "error" not in result
        assert result["total"] >= 1

    async def test_search_knowledge(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.knowledge.*",
            query="api",
            project_path=str(temp_project),
            space="project",
        )

        assert "error" not in result
        assert result["total"] >= 1

    async def test_search_empty_query(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query="",
            project_path=str(temp_project),
            space="project",
        )

        assert "error" not in result
        assert result["total"] >= 2

    async def test_search_with_limit(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query="",
            project_path=str(temp_project),
            space="project",
            limit=1,
        )

        assert "error" not in result
        assert len(result["results"]) <= 1

    async def test_search_nonexistent_project(self):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query="test",
            project_path="/nonexistent/path",
            space="project",
        )

        assert "error" not in result
        assert result["total"] == 0

    async def test_boolean_or(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query="create OR bootstrap",
            project_path=str(temp_project),
            space="project",
        )

        assert "error" not in result
        assert result["total"] >= 2

    async def test_boolean_not(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query="NOT bootstrap",
            project_path=str(temp_project),
            space="project",
        )

        assert "error" not in result
        assert all(
            "bootstrap" not in r.get("name", "") for r in result["results"]
        )

    async def test_phrase_search(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query='"Create a new tool"',
            project_path=str(temp_project),
            space="project",
        )

        assert "error" not in result
        assert result["total"] >= 1

    async def test_wildcard_search(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query="boot*",
            project_path=str(temp_project),
            space="project",
        )

        assert "error" not in result
        assert result["total"] >= 1

    async def test_field_specific_search(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query="",
            fields={"name": "bootstrap"},
            project_path=str(temp_project),
            space="project",
        )

        assert "error" not in result
        assert result["total"] >= 1
        assert result["results"][0]["name"] == "bootstrap"

    async def test_pagination_offset(self, temp_project):
        tool = SearchTool("")
        all_results = await tool.handle(
            scope="rye.search.directive.*",
            query="",
            project_path=str(temp_project),
            space="project",
            limit=100,
        )
        page2 = await tool.handle(
            scope="rye.search.directive.*",
            query="",
            project_path=str(temp_project),
            space="project",
            limit=1,
            offset=1,
        )

        assert page2["total"] == all_results["total"]
        assert len(page2["results"]) <= 1

    async def test_response_schema(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            scope="rye.search.directive.*",
            query="create",
            project_path=str(temp_project),
            space="project",
        )

        assert "results" in result
        assert "total" in result
        assert "query" in result
        assert "scope" in result
        assert "space" in result
        assert "limit" in result
        assert "offset" in result
        assert "search_type" in result
        assert result["search_type"] == "keyword"

        if result["results"]:
            item = result["results"][0]
            assert "id" in item
            assert "type" in item
            assert "score" in item
            assert "preview" in item
            assert "source" in item
            assert "path" in item
