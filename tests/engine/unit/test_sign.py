"""Tests for sign tool."""

import asyncio
import tempfile
from pathlib import Path

import pytest

from rye.actions.sign import SignTool


@pytest.fixture
def temp_project(_setup_user_space):
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
            '      <execute tool="rye/bash/bash" />\n'
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
            '<!-- rye:unsigned -->\n\n'
            '```yaml\nname: entry\ntitle: Test\nentry_type: note\n'
            'version: "1.0.0"\nauthor: test\ncreated_at: 2026-01-01T00:00:00Z\n```\n\nContent'
        )

        yield project_root


@pytest.mark.asyncio
class TestSignTool:
    """Test sign tool."""

    async def test_sign_directive(self, temp_project):
        """Sign directive file."""
        tool = SignTool("")
        result = await tool.handle(
            item_id="directive:test",
            project_path=str(temp_project),
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
            item_id="tool:test/script",
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
            item_id="knowledge:entry",
            project_path=str(temp_project),
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
            item_id="tool:test/script",
            project_path=str(temp_project),
        )
        sig1 = result1["signature"]

        # Sign again
        result2 = await tool.handle(
            item_id="tool:test/script",
            project_path=str(temp_project),
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
            item_id="directive:nonexistent",
            project_path=str(temp_project),
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
            item_id="directive:invalid",
            project_path=str(temp_project),
        )

        assert result["status"] == "error"
        assert (
            "invalid" in result["error"].lower()
            or "validation" in result["error"].lower()
        )


@pytest.mark.asyncio
class TestSignNotFoundErrors:
    """Not-found errors include searched_in path and hint about project_path."""

    async def test_directive_not_found_shows_searched_path(self, _setup_user_space):
        """Error should include the path that was searched."""
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / ".ai" / "directives").mkdir(parents=True)

            tool = SignTool("")
            result = await tool.handle(
                item_id="directive:missing",
                project_path=str(root),
                source="project",
            )

            assert result["status"] == "error"
            assert "searched_in" in result
            assert ".ai/directives" in result["searched_in"]
            assert "parent of the .ai/" in result["hint"]

    async def test_tool_not_found_shows_searched_path(self, _setup_user_space):
        """Error should include the path that was searched."""
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / ".ai" / "tools").mkdir(parents=True)

            tool = SignTool("")
            result = await tool.handle(
                item_id="tool:my/tool",
                project_path=str(root),
                source="project",
            )

            assert result["status"] == "error"
            assert "searched_in" in result
            assert ".ai/tools" in result["searched_in"]

    async def test_knowledge_not_found_shows_searched_path(self, _setup_user_space):
        """Error should include the path that was searched."""
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            (root / ".ai" / "knowledge").mkdir(parents=True)

            tool = SignTool("")
            result = await tool.handle(
                item_id="knowledge:topic",
                project_path=str(root),
                source="project",
            )

            assert result["status"] == "error"
            assert "searched_in" in result
            assert ".ai/knowledge" in result["searched_in"]


@pytest.mark.asyncio
class TestSignCanonicalRefs:
    """Canonical refs (tool:id, directive:id) derive item_type from prefix."""

    async def test_sign_tool_via_canonical_ref(self, temp_project):
        """Sign tool using canonical ref instead of item_type."""
        tool = SignTool("")
        result = await tool.handle(
            item_id="tool:test/script",
            project_path=str(temp_project),
            source="project",
        )
        assert result["status"] == "signed"

    async def test_sign_directive_via_canonical_ref(self, temp_project):
        """Sign directive using canonical ref instead of item_type."""
        tool = SignTool("")
        result = await tool.handle(
            item_id="directive:test",
            project_path=str(temp_project),
            source="project",
        )
        assert result["status"] == "signed"

    async def test_sign_knowledge_via_canonical_ref(self, temp_project):
        """Sign knowledge using canonical ref instead of item_type."""
        tool = SignTool("")
        result = await tool.handle(
            item_id="knowledge:entry",
            project_path=str(temp_project),
            source="project",
        )
        assert result["status"] == "signed"

    async def test_canonical_ref_overrides_item_type(self, temp_project):
        """Canonical ref kind is derived from prefix."""
        tool = SignTool("")
        result = await tool.handle(
            item_id="tool:test/script",
            project_path=str(temp_project),
            source="project",
        )
        assert result["status"] == "signed"

    async def test_missing_item_type_and_no_prefix_errors(self, temp_project):
        """Error when no canonical prefix."""
        tool = SignTool("")
        result = await tool.handle(
            item_id="test/script",
            project_path=str(temp_project),
            source="project",
        )
        assert result["status"] == "error"
        assert "canonical ref" in result["error"].lower()
