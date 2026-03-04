"""Tests for improved integrity error messages and dev mode."""

import os
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from rye.utils.integrity import verify_item, IntegrityError, _infer_item_id
from rye.constants import ItemType
from rye.tools.execute import ExecuteTool
from rye.tools.load import LoadTool


class TestInferItemId:
    """Test _infer_item_id extracts correct item IDs from paths."""

    def test_tool_path(self):
        path = Path("/project/.ai/tools/rye/bash/bash.py")
        assert _infer_item_id(path, ItemType.TOOL, None) == "rye/bash/bash"

    def test_directive_path(self):
        path = Path("/project/.ai/directives/my/workflow.md")
        assert _infer_item_id(path, ItemType.DIRECTIVE, None) == "my/workflow"

    def test_knowledge_path(self):
        path = Path("/project/.ai/knowledge/rye/core/something.md")
        assert _infer_item_id(path, ItemType.KNOWLEDGE, None) == "rye/core/something"

    def test_no_ai_dir_fallback(self):
        path = Path("/some/random/file.py")
        assert _infer_item_id(path, ItemType.TOOL, None) == "file"

    def test_nested_ai_dir(self):
        path = Path("/home/user/.ai/tools/custom/tool.py")
        assert _infer_item_id(path, ItemType.TOOL, None) == "custom/tool"


class TestIntegrityErrorMessages:
    """Test that integrity errors include actionable context."""

    def test_unsigned_error_includes_fix_command(self, _setup_user_space):
        """Unsigned items should suggest the rye sign command."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            tools_dir = project / ".ai" / "tools" / "my"
            tools_dir.mkdir(parents=True)
            tool_file = tools_dir / "tool.py"
            tool_file.write_text('__version__ = "1.0.0"\n')

            with pytest.raises(IntegrityError, match=r"rye sign tool my/tool"):
                verify_item(tool_file, ItemType.TOOL, project_path=project)

    def test_unsigned_error_includes_item_type(self, _setup_user_space):
        """Error should mention the item type."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            tools_dir = project / ".ai" / "tools"
            tools_dir.mkdir(parents=True)
            tool_file = tools_dir / "test.py"
            tool_file.write_text("pass\n")

            with pytest.raises(IntegrityError, match=r"Item type: tool"):
                verify_item(tool_file, ItemType.TOOL, project_path=project)

    def test_unsigned_error_mentions_expected_header(self, _setup_user_space):
        """Error should mention the expected signature header."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            tools_dir = project / ".ai" / "tools"
            tools_dir.mkdir(parents=True)
            tool_file = tools_dir / "test.py"
            tool_file.write_text("pass\n")

            with pytest.raises(IntegrityError, match=r"rye:signed:"):
                verify_item(tool_file, ItemType.TOOL, project_path=project)

    def test_hash_mismatch_error_includes_fix(self, _setup_user_space):
        """Content-modified errors should suggest re-signing."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            tools_dir = project / ".ai" / "tools"
            tools_dir.mkdir(parents=True)
            tool_file = tools_dir / "test.py"

            # Sign it properly first
            from rye.utils.metadata_manager import MetadataManager
            content = '__version__ = "1.0.0"\n'
            signed = MetadataManager.sign_content(
                ItemType.TOOL, content, file_path=tool_file, project_path=project
            )
            tool_file.write_text(signed)

            # Tamper with content (preserve signature line)
            lines = signed.split("\n")
            lines[1] = "# tampered"
            tool_file.write_text("\n".join(lines))

            with pytest.raises(IntegrityError, match=r"Re-sign after editing"):
                verify_item(tool_file, ItemType.TOOL, project_path=project)


class TestDevMode:
    """Test RYE_DEV_MODE=1 downgrades IntegrityError to warning."""

    def test_dev_mode_returns_unverified(self, _setup_user_space):
        """Dev mode should return 'unverified' instead of raising."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            tools_dir = project / ".ai" / "tools"
            tools_dir.mkdir(parents=True)
            tool_file = tools_dir / "test.py"
            tool_file.write_text("pass\n")

            with patch.dict(os.environ, {"RYE_DEV_MODE": "1"}):
                result = verify_item(tool_file, ItemType.TOOL, project_path=project)
                assert result == "unverified"

    def test_dev_mode_off_raises(self, _setup_user_space):
        """Without dev mode, unsigned items should raise."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            tools_dir = project / ".ai" / "tools"
            tools_dir.mkdir(parents=True)
            tool_file = tools_dir / "test.py"
            tool_file.write_text("pass\n")

            with patch.dict(os.environ, {}, clear=False):
                os.environ.pop("RYE_DEV_MODE", None)
                with pytest.raises(IntegrityError):
                    verify_item(tool_file, ItemType.TOOL, project_path=project)

    def test_dev_mode_value_must_be_1(self, _setup_user_space):
        """Only '1' activates dev mode, not 'true' or other values."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            tools_dir = project / ".ai" / "tools"
            tools_dir.mkdir(parents=True)
            tool_file = tools_dir / "test.py"
            tool_file.write_text("pass\n")

            with patch.dict(os.environ, {"RYE_DEV_MODE": "true"}):
                with pytest.raises(IntegrityError):
                    verify_item(tool_file, ItemType.TOOL, project_path=project)


class TestExecuteToolIntegrityErrorType:
    """Test that ExecuteTool.handle() propagates error_type='integrity' for IntegrityErrors."""

    @pytest.mark.asyncio
    async def test_execute_unsigned_knowledge_returns_integrity_error_type(self, _setup_user_space):
        """Unsigned knowledge item should return error_type='integrity'."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            knowledge_dir = project / ".ai" / "knowledge"
            knowledge_dir.mkdir(parents=True)
            (knowledge_dir / "unsigned_entry.md").write_text(
                "---\ntitle: Unsigned\nname: unsigned_entry\n---\n\nSome content"
            )

            tool = ExecuteTool(project_path=str(project))
            result = await tool.handle(
                item_type=ItemType.KNOWLEDGE,
                item_id="unsigned_entry",
                project_path=str(project),
            )
            assert result["status"] == "error"
            assert result["error_type"] == "integrity"
            assert result["item_id"] == "unsigned_entry"

    @pytest.mark.asyncio
    async def test_load_unsigned_directive_returns_integrity_error_type(self, _setup_user_space):
        """LoadTool.handle() for an unsigned directive should return error_type='integrity'."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            directives_dir = project / ".ai" / "directives"
            directives_dir.mkdir(parents=True)
            (directives_dir / "unsigned_workflow.md").write_text(
                "# Unsigned Workflow\n\nSome directive content"
            )

            loader = LoadTool()
            result = await loader.handle(
                item_type=ItemType.DIRECTIVE,
                item_id="unsigned_workflow",
                project_path=str(project),
            )
            assert result["status"] == "error"
            assert result["error_type"] == "integrity"
            assert result["item_id"] == "unsigned_workflow"

    @pytest.mark.asyncio
    async def test_execute_non_integrity_error_has_no_error_type(self, _setup_user_space):
        """Non-existent item should error without error_type key."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "knowledge").mkdir(parents=True)

            tool = ExecuteTool(project_path=str(project))
            result = await tool.handle(
                item_type=ItemType.KNOWLEDGE,
                item_id="nonexistent_item",
                project_path=str(project),
            )
            assert result["status"] == "error"
            assert "error_type" not in result
