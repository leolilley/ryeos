"""Tests for search tool."""

import asyncio
import tempfile
from pathlib import Path

import pytest

from rye.tools.search import SearchTool


@pytest.fixture
def temp_project():
    """Create temporary project with test items."""
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        ai_dir = project_root / ".ai"

        directives_dir = ai_dir / "directives"
        directives_dir.mkdir(parents=True)
        (directives_dir / "create_tool.md").write_text(
            '<directive name="create_tool" version="1.0.0">\n'
            '<metadata><description>Create a new tool</description></metadata>\n'
            '</directive>'
        )
        (directives_dir / "bootstrap.md").write_text(
            '<directive name="bootstrap" version="1.0.0">\n'
            '<metadata><description>Bootstrap project</description></metadata>\n'
            '</directive>'
        )

        tools_dir = ai_dir / "tools"
        tools_dir.mkdir(parents=True)
        (tools_dir / "scraper.py").write_text('# Tool scraper\n' "# version=1.0.0")

        knowledge_dir = ai_dir / "knowledge"
        knowledge_dir.mkdir(parents=True)
        (knowledge_dir / "api_patterns.md").write_text(
            "---\ntitle: API Design Patterns\n---\n\nAPI patterns content"
        )

        yield project_root


@pytest.mark.asyncio
class TestSearchTool:
    """Test search tool."""

    async def test_search_directives(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query="create",
            project_path=str(temp_project),
            source="project",
        )

        assert "error" not in result
        assert result["total"] >= 1
        assert any(r["name"] == "create_tool" for r in result["results"])

    async def test_search_tools(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="tool",
            query="scraper",
            project_path=str(temp_project),
            source="project",
        )

        assert "error" not in result
        assert result["total"] >= 1

    async def test_search_knowledge(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="knowledge",
            query="api",
            project_path=str(temp_project),
            source="project",
        )

        assert "error" not in result
        assert result["total"] >= 1

    async def test_search_empty_query(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query="",
            project_path=str(temp_project),
            source="project",
        )

        assert "error" not in result
        assert result["total"] >= 2

    async def test_search_with_limit(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query="",
            project_path=str(temp_project),
            source="project",
            limit=1,
        )

        assert "error" not in result
        assert len(result["results"]) <= 1

    async def test_search_nonexistent_project(self):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query="test",
            project_path="/nonexistent/path",
            source="project",
        )

        assert "error" not in result
        assert result["total"] == 0

    async def test_boolean_or(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query="create OR bootstrap",
            project_path=str(temp_project),
            source="project",
        )

        assert "error" not in result
        assert result["total"] >= 2

    async def test_boolean_not(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query="NOT bootstrap",
            project_path=str(temp_project),
            source="project",
        )

        assert "error" not in result
        assert all(
            "bootstrap" not in r.get("name", "") for r in result["results"]
        )

    async def test_phrase_search(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query='"Create a new tool"',
            project_path=str(temp_project),
            source="project",
        )

        assert "error" not in result
        assert result["total"] >= 1

    async def test_wildcard_search(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query="boot*",
            project_path=str(temp_project),
            source="project",
        )

        assert "error" not in result
        assert result["total"] >= 1

    async def test_field_specific_search(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query="",
            fields={"name": "bootstrap"},
            project_path=str(temp_project),
            source="project",
        )

        assert "error" not in result
        assert result["total"] >= 1
        assert result["results"][0]["name"] == "bootstrap"

    async def test_pagination_offset(self, temp_project):
        tool = SearchTool("")
        all_results = await tool.handle(
            item_type="directive",
            query="",
            project_path=str(temp_project),
            source="project",
            limit=100,
        )
        page2 = await tool.handle(
            item_type="directive",
            query="",
            project_path=str(temp_project),
            source="project",
            limit=1,
            offset=1,
        )

        assert page2["total"] == all_results["total"]
        assert len(page2["results"]) <= 1

    async def test_response_schema(self, temp_project):
        tool = SearchTool("")
        result = await tool.handle(
            item_type="directive",
            query="create",
            project_path=str(temp_project),
            source="project",
        )

        assert "results" in result
        assert "total" in result
        assert "query" in result
        assert "item_type" in result
        assert "source" in result
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
