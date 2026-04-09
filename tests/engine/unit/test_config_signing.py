"""Tests for config signing support (Step 4a)."""

import os
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from rye.utils.metadata_manager import MetadataManager, ToolMetadataStrategy
from rye.utils.integrity import verify_item, IntegrityError
from rye.utils.execution_context import ExecutionContext


class TestConfigMetadataStrategy:
    """Test that kind='config' routes to ToolMetadataStrategy."""

    def test_get_strategy_returns_tool_strategy_for_config(self):
        strategy = MetadataManager.get_strategy("config")
        assert isinstance(strategy, ToolMetadataStrategy)

    def test_config_strategy_has_correct_item_type(self):
        strategy = MetadataManager.get_strategy("config")
        assert strategy._kind == "config"

    def test_tool_strategy_default_kind(self):
        """Default kind remains 'tool' for backward compatibility."""
        strategy = ToolMetadataStrategy()
        assert strategy._kind == "tool"

    def test_trust_store_strategy_unchanged(self):
        """trust_store.py creates ToolMetadataStrategy() with no args — must still work."""
        strategy = ToolMetadataStrategy()
        assert strategy._kind == "tool"
        # Default sig format should work
        fmt = strategy._get_signature_format()
        assert fmt["prefix"] == "#"


class TestVerifyItemAllowUnsigned:
    """Test allow_unsigned parameter on verify_item."""

    def test_unsigned_config_allowed(self, _setup_user_space):
        """allow_unsigned=True returns 'unsigned' for unsigned files."""
        with tempfile.TemporaryDirectory() as tmpdir:
            config_file = Path(tmpdir) / "test.yaml"
            config_file.write_text("key: value\n")

            result = verify_item(
                config_file, "config",
                ctx=ExecutionContext.from_env(project_path=Path(tmpdir)),
                allow_unsigned=True,
            )
            assert result == "unsigned"

    def test_unsigned_config_rejected_by_default(self, _setup_user_space):
        """Without allow_unsigned, unsigned configs raise IntegrityError."""
        with tempfile.TemporaryDirectory() as tmpdir:
            config_file = Path(tmpdir) / "test.yaml"
            config_file.write_text("key: value\n")

            with pytest.raises(IntegrityError):
                verify_item(config_file, "config", ctx=ExecutionContext.from_env(project_path=Path(tmpdir)))

    def test_tampered_config_rejected_even_with_allow_unsigned(self, _setup_user_space):
        """Tampered signed config is always rejected."""
        with tempfile.TemporaryDirectory() as tmpdir:
            config_file = Path(tmpdir) / "test.yaml"
            content = "key: value\n"

            # Sign it
            signed = MetadataManager.sign_content(
                "config", content, file_path=config_file, project_path=Path(tmpdir)
            )
            config_file.write_text(signed)

            # Tamper
            lines = signed.split("\n")
            lines[1] = "key: tampered"
            config_file.write_text("\n".join(lines))

            with pytest.raises(IntegrityError, match="modified since signing"):
                verify_item(
                    config_file, "config",
                    ctx=ExecutionContext.from_env(project_path=Path(tmpdir)),
                    allow_unsigned=True,
                )

    def test_allow_unsigned_false_is_default(self, _setup_user_space):
        """Ensure backward compatibility — default is False."""
        with tempfile.TemporaryDirectory() as tmpdir:
            tool_file = Path(tmpdir) / ".ai" / "tools" / "test.py"
            tool_file.parent.mkdir(parents=True)
            tool_file.write_text("pass\n")

            with pytest.raises(IntegrityError):
                verify_item(tool_file, "tool", ctx=ExecutionContext.from_env(project_path=Path(tmpdir)))
