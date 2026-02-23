"""Tests for sign tool."""

import asyncio
import tempfile
from pathlib import Path

import pytest

from rye.tools.sign import SignTool


@pytest.fixture
def temp_project():
    """Create temporary project with test items."""
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        ai_dir = project_root / ".ai"

        # Create directive with all required fields
        directives_dir = ai_dir / "directives"
        directives_dir.mkdir(parents=True)
        (directives_dir / "test.md").write_text(
            "# Test Directive\n\n"
            "```xml\n"
            '<directive name="test" version="1.0.0">\n'
            "  <metadata>\n"
            "    <description>Test directive</description>\n"
            "    <category />\n"
            "    <author>test</author>\n"
            '    <model tier="general" />\n'
            "    <permissions>\n"
            '      <read resource="filesystem" />\n'
            "    </permissions>\n"
            "  </metadata>\n"
            "</directive>\n"
            "```\n"
        )

        # Create tool with required metadata
        tools_dir = ai_dir / "tools"
        test_tools_dir = tools_dir / "test"
        test_tools_dir.mkdir(parents=True)
        (test_tools_dir / "script.py").write_text(
            '"""Test script tool"""\n'
            "__version__ = '1.0.0'\n"
            "__tool_type__ = 'script'\n"
            "__executor_id__ = 'python_runtime'\n"
            "__category__ = 'test'\n"
            "__tool_description__ = 'Test script for signing'\n"
            "print('test')"
        )

        # Create knowledge with all required fields
        knowledge_dir = ai_dir / "knowledge"
        knowledge_dir.mkdir(parents=True)
        (knowledge_dir / "entry.md").write_text(
            '---\ntitle: Test\nname: entry\nversion: "1.0.0"\nentry_type: note\n---\n\nContent'
        )

        yield project_root


@pytest.mark.asyncio
class TestSignTool:
    """Test sign tool."""

    async def test_sign_directive(self, temp_project):
        """Sign directive file."""
        tool = SignTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="test",
            project_path=str(temp_project),
            location="project",
        )

        assert result["status"] == "signed"
        assert "signature" in result

        # Verify signature was added to file
        directive_file = temp_project / ".ai" / "directives" / "test.md"
        content = directive_file.read_text()
        assert "rye:signed:" in content

    async def test_sign_tool(self, temp_project):
        """Sign tool file."""
        tool = SignTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="test/script",  # Full relative path from .ai/tools/
            project_path=str(temp_project),
            source="project",
        )

        assert result["status"] == "signed"

        # Verify signature format (code comment for .py)
        tool_file = temp_project / ".ai" / "tools" / "test" / "script.py"
        content = tool_file.read_text()
        assert "# rye:signed:" in content

    async def test_sign_knowledge(self, temp_project):
        """Sign knowledge file."""
        tool = SignTool("")
        result = await tool.handle(
            item_type="knowledge",
            item_id="entry",
            project_path=str(temp_project),
            location="project",
        )

        assert result["status"] == "signed"

        # Verify signature format (HTML comment for .md)
        entry_file = temp_project / ".ai" / "knowledge" / "entry.md"
        content = entry_file.read_text()
        assert "<!-- rye:signed:" in content

    async def test_resign_item(self, temp_project):
        """Resign item replaces old signature."""
        tool = SignTool("")

        # Sign once
        result1 = await tool.handle(
            item_type="tool",
            item_id="test/script",  # Full relative path from .ai/tools/
            project_path=str(temp_project),
            location="project",
        )
        sig1 = result1["signature"]

        # Sign again
        result2 = await tool.handle(
            item_type="tool",
            item_id="test/script",  # Full relative path from .ai/tools/
            project_path=str(temp_project),
            location="project",
        )
        sig2 = result2["signature"]

        # File should only have one signature
        tool_file = temp_project / ".ai" / "tools" / "test" / "script.py"
        content = tool_file.read_text()
        assert content.count("rye:signed:") == 1

    async def test_sign_nonexistent_item(self, temp_project):
        """Error on nonexistent item."""
        tool = SignTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="nonexistent",
            project_path=str(temp_project),
            location="project",
        )

        assert result["status"] == "error"
        assert "not found" in result["error"].lower()

    async def test_sign_invalid_directive(self, temp_project):
        """Error on invalid directive structure."""
        # Create invalid directive
        invalid_file = temp_project / ".ai" / "directives" / "invalid.md"
        invalid_file.write_text("This is not XML")

        tool = SignTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="invalid",
            project_path=str(temp_project),
            location="project",
        )

        assert result["status"] == "error"
        assert (
            "invalid" in result["error"].lower()
            or "validation" in result["error"].lower()
        )
