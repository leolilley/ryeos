"""Tests for executor chain trace mode and shadow detection."""

import tempfile
from pathlib import Path

import pytest

from rye.executor.primitive_executor import PrimitiveExecutor, ExecutionResult
from rye.utils.execution_context import ExecutionContext
from rye.utils.path_utils import BundleInfo


class TestExecutionResultTrace:
    """Test ExecutionResult trace field."""

    def test_trace_field_defaults_empty(self):
        result = ExecutionResult(success=True)
        assert result.trace == []

    def test_trace_field_accepts_events(self):
        events = [
            {"step": "resolve", "item_id": "test", "space": "project"},
            {"step": "verify_integrity", "item_id": "test", "verified": True},
        ]
        result = ExecutionResult(success=True, trace=events)
        assert len(result.trace) == 2
        assert result.trace[0]["step"] == "resolve"


class TestFindShadowedPaths:
    """Test PrimitiveExecutor._find_shadowed_paths."""

    def test_no_shadows_when_only_one_space(self):
        """Item only in project has nothing shadowed."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            tools_dir = project / ".ai" / "tools" / "my"
            tools_dir.mkdir(parents=True)
            (tools_dir / "tool.py").write_text("pass\n")

            executor = PrimitiveExecutor(ctx=ExecutionContext(
                project_path=project,
                user_space=project / "user",  # doesn't exist
                signing_key_dir=project / "keys",
                system_spaces=(BundleInfo(bundle_id="test", version="0.0.0", root_path=project / "system", manifest_path=None, source="test"),),
            ))
            shadowed = executor._find_shadowed_paths("my/tool", "project")
            assert shadowed == []

    def test_finds_shadowed_in_system(self):
        """Project item that shadows a system item."""
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)

            # Project tool
            project_tools = root / "project" / ".ai" / "tools" / "my"
            project_tools.mkdir(parents=True)
            (project_tools / "tool.py").write_text("# project\n")

            # System tool with same ID
            system_tools = root / "system" / ".ai" / "tools" / "my"
            system_tools.mkdir(parents=True)
            (system_tools / "tool.py").write_text("# system\n")

            executor = PrimitiveExecutor(ctx=ExecutionContext(
                project_path=root / "project",
                user_space=root / "user",
                signing_key_dir=root / "keys",
                system_spaces=(BundleInfo(bundle_id="test", version="0.0.0", root_path=root / "system", manifest_path=None, source="test"),),
            ))
            shadowed = executor._find_shadowed_paths("my/tool", "project")
            assert len(shadowed) == 1
            assert "system" in shadowed[0]["space"]

    def test_no_shadows_when_found_in_system(self):
        """System item has nothing below it to shadow."""
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)

            system_tools = root / "system" / ".ai" / "tools" / "my"
            system_tools.mkdir(parents=True)
            (system_tools / "tool.py").write_text("# system\n")

            executor = PrimitiveExecutor(ctx=ExecutionContext(
                project_path=root / "project",
                user_space=root / "user",
                signing_key_dir=root / "keys",
                system_spaces=(BundleInfo(bundle_id="ryeos", version="0.0.0", root_path=root / "system", manifest_path=None, source="test"),),
            ))
            # system:ryeos matches the bundle ID format
            shadowed = executor._find_shadowed_paths("my/tool", "system:ryeos")
            assert shadowed == []
