"""Tests for load tool."""

import asyncio
import tempfile
from pathlib import Path

import pytest

from rye.tools.load import LoadTool


@pytest.fixture
def temp_project(_setup_user_space):
    """Create temporary project with test items."""
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        ai_dir = project_root / ".ai"

        # Create directive
        directives_dir = ai_dir / "directives"
        directives_dir.mkdir(parents=True)
        directive_content = (
            '<directive name="test" version="1.0.0">'
            '<metadata><description>Test directive</description></metadata>'
            '</directive>'
        )
        (directives_dir / "test.md").write_text(directive_content)

        # Create tool
        tools_dir = ai_dir / "tools"
        tools_dir.mkdir(parents=True)
        (tools_dir / "myscript.py").write_text("# Tool script\nversion=1.0.0")

        from rye.utils.metadata_manager import MetadataManager
        from rye.constants import ItemType

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

        yield project_root


@pytest.fixture
def temp_user_space(_setup_user_space):
    """Create temporary user space."""
    with tempfile.TemporaryDirectory() as tmpdir:
        user_space = Path(tmpdir)

        # Create user tool
        tools_dir = user_space / ".ai" / "tools"
        tools_dir.mkdir(parents=True)
        (tools_dir / "shared.py").write_text("# Shared tool\nversion=1.0.0")

        from rye.utils.metadata_manager import MetadataManager
        from rye.constants import ItemType

        for tool_file in (tools_dir).rglob("*.py"):
            content = tool_file.read_text()
            signed = MetadataManager.sign_content(
                ItemType.TOOL, content, file_path=tool_file
            )
            tool_file.write_text(signed)

        yield user_space


@pytest.mark.asyncio
class TestLoadTool:
    """Test load tool."""

    async def test_load_directive(self, temp_project):
        """Load directive content."""
        tool = LoadTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="test",
            project_path=str(temp_project),
            source="project",
        )

        assert result["status"] == "success"
        assert '<directive name="test"' in result["content"]
        assert "Test directive" in result["content"]

    async def test_load_tool(self, temp_project):
        """Load tool content."""
        tool = LoadTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="myscript",
            project_path=str(temp_project),
            source="project",
        )

        assert result["status"] == "success"
        assert "Tool script" in result["content"]

    async def test_load_nonexistent_item(self, temp_project):
        """Handle nonexistent item."""
        tool = LoadTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="nonexistent",
            project_path=str(temp_project),
            source="project",
        )

        assert result["status"] == "error"
        assert "not found" in result["error"].lower()

    async def test_load_with_metadata(self, temp_project):
        """Load extracts metadata."""
        tool = LoadTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="test",
            project_path=str(temp_project),
            source="project",
        )

        assert result["status"] == "success"
        assert "metadata" in result
        assert result["metadata"]["name"] == "test"

    async def test_copy_to_user_space(self, temp_project, temp_user_space):
        """Copy item from project to user space."""
        tool = LoadTool(str(temp_user_space))
        result = await tool.handle(
            item_type="tool",
            item_id="myscript",
            project_path=str(temp_project),
            source="project",
            destination="user",
        )

        assert result["status"] == "success"
        assert result["copied_to"] == "user"
        
        # Verify file was copied (use glob since extension may vary)
        tools_dir = temp_user_space / ".ai" / "tools"
        assert tools_dir.exists()
        files = list(tools_dir.glob("myscript*"))
        assert len(files) > 0, f"No myscript file found in {tools_dir}"

    async def test_load_from_user_space(self, temp_user_space):
        """Load item from user space."""
        tool = LoadTool(str(temp_user_space))
        result = await tool.handle(
            item_type="tool",
            item_id="shared",
            project_path="/dummy",
            source="user",
        )

        assert result["status"] == "success"
        assert "Shared tool" in result["content"]
