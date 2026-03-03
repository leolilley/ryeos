"""Tests for tool schema preload (Layer 1), resolve_extends (Layer 2),
base directives (Layer 3), and hook removal (Phase 4)."""

import importlib.util
import sys
import tempfile
from pathlib import Path

import pytest
import yaml

from conftest import PROJECT_ROOT, get_bundle_path

_STD_ROOT = PROJECT_ROOT / "ryeos" / "bundles" / "standard" / "ryeos_std"

# Load tool_schema_loader module for testing
_TSL_PATH = get_bundle_path(
    "standard", "tools/rye/agent/threads/loaders/tool_schema_loader.py"
)
_tsl_spec = importlib.util.spec_from_file_location("tool_schema_loader", _TSL_PATH)
_tsl = importlib.util.module_from_spec(_tsl_spec)
_tsl_spec.loader.exec_module(_tsl)

# Load condition_evaluator for resolve_extends tests
_CE_PATH = get_bundle_path(
    "standard", "tools/rye/agent/threads/loaders/condition_evaluator.py"
)
_ce_spec = importlib.util.spec_from_file_location("condition_evaluator_test", _CE_PATH)
_ce = importlib.util.module_from_spec(_ce_spec)
_ce_spec.loader.exec_module(_ce)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

SAMPLE_TOOL = '''\
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "test/tools"
__tool_description__ = "Run bash commands"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "command": {
            "type": "string",
            "description": "The command to execute",
        },
        "timeout": {
            "type": "integer",
            "description": "Max seconds to wait",
        },
    },
    "required": ["command"],
}
'''

SAMPLE_TOOL_NO_SCHEMA = '''\
__version__ = "1.0.0"
__tool_type__ = "python"
__tool_description__ = "No schema tool"

def execute(params, project_path):
    return {"success": True}
'''


@pytest.fixture
def tool_project(tmp_path):
    """Project with tools laid out for 3-tier resolution."""
    tools = tmp_path / ".ai" / "tools"

    # rye/bash/bash.py
    bash_dir = tools / "rye" / "bash"
    bash_dir.mkdir(parents=True)
    (bash_dir / "bash.py").write_text(SAMPLE_TOOL)

    # rye/file-system/read.py
    fs_dir = tools / "rye" / "file-system"
    fs_dir.mkdir(parents=True)
    (fs_dir / "read.py").write_text(
        SAMPLE_TOOL.replace("Run bash commands", "Read files")
        .replace('"command"', '"path"')
        .replace("The command to execute", "File path to read")
        .replace('"timeout"', '"encoding"')
        .replace("Max seconds to wait", "File encoding")
        .replace('"integer"', '"string"')
    )

    # rye/primary/rye_execute.py — should be skipped
    primary_dir = tools / "rye" / "primary"
    primary_dir.mkdir(parents=True)
    (primary_dir / "rye_execute.py").write_text(SAMPLE_TOOL)

    return tmp_path


# ---------------------------------------------------------------------------
# Phase 1: tool_schema_loader
# ---------------------------------------------------------------------------

class TestParseCapability:
    def test_execute_tool_cap(self):
        assert _tsl._parse_capability("rye.execute.tool.rye.bash.*") == "rye/bash/*"
        assert _tsl._parse_capability("rye.execute.tool.rye.file-system.read") == "rye/file-system/read"

    def test_non_tool_cap_returns_none(self):
        assert _tsl._parse_capability("rye.search.tool") is None
        assert _tsl._parse_capability("rye.load.knowledge.foo") is None
        assert _tsl._parse_capability("rye.sign.directive.bar") is None

    def test_empty_remainder_returns_none(self):
        assert _tsl._parse_capability("rye.execute.tool.") is None


class TestPatternSpecificity:
    def test_exact_beats_wildcard(self):
        assert _tsl._pattern_specificity("rye/file-system/read") == 1000
        assert _tsl._pattern_specificity("rye/file-system/*") < 1000

    def test_deeper_wildcard_beats_shallow(self):
        assert _tsl._pattern_specificity("rye/file-system/*") > _tsl._pattern_specificity("rye/*")


class TestExtractToolMetadata:
    def test_extracts_schema_and_description(self, tmp_path):
        tool_file = tmp_path / "test_tool.py"
        tool_file.write_text(SAMPLE_TOOL)

        meta = _tsl._extract_tool_metadata(tool_file)
        assert meta is not None
        assert meta["description"] == "Run bash commands"
        assert "command" in meta["schema"]["properties"]
        assert "command" in meta["schema"]["required"]

    def test_returns_none_without_schema(self, tmp_path):
        tool_file = tmp_path / "no_schema.py"
        tool_file.write_text(SAMPLE_TOOL_NO_SCHEMA)
        assert _tsl._extract_tool_metadata(tool_file) is None

    def test_returns_none_for_syntax_error(self, tmp_path):
        tool_file = tmp_path / "bad.py"
        tool_file.write_text("def broken(:\n")
        assert _tsl._extract_tool_metadata(tool_file) is None


class TestFormatSchemaBlock:
    def test_format_includes_required_tag(self):
        meta = {
            "description": "Test tool",
            "schema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The name"},
                    "count": {"type": "integer"},
                },
                "required": ["name"],
            },
        }
        block = _tsl._format_schema_block("test/my_tool", meta)
        assert 'rye_execute(item_type="tool", item_id="test/my_tool"' in block
        assert '"name": "<string>"' in block
        assert '"count": "<integer>"' in block
        assert "name (string) [required]: The name" in block
        assert "count (integer)" in block
        # count should NOT have [required]
        count_line = [l for l in block.split("\n") if "count" in l and "(" in l][0]
        assert "[required]" not in count_line


class TestPreloadToolSchemas:
    def test_preloads_matching_tools(self, tool_project):
        from unittest.mock import patch
        mock_paths = [(tool_project / ".ai" / "tools", "project")]
        with patch.object(_tsl.ToolResolver, "get_search_paths", return_value=mock_paths):
            with patch.object(_tsl, "get_tool_extensions", return_value=[".py"]):
                result = _tsl.preload_tool_schemas(
                    ["rye.execute.tool.rye.bash.*"], tool_project,
                )

        assert result["schemas"]
        assert "rye/bash/bash" in result["preloaded_tools"]
        assert "Available tools (call via rye_execute):" in result["schemas"]
        assert 'item_id="rye/bash/bash"' in result["schemas"]
        assert "Run bash commands" in result["schemas"]

    def test_skips_primary_tools(self, tool_project):
        from unittest.mock import patch
        mock_paths = [(tool_project / ".ai" / "tools", "project")]
        with patch.object(_tsl.ToolResolver, "get_search_paths", return_value=mock_paths):
            with patch.object(_tsl, "get_tool_extensions", return_value=[".py"]):
                result = _tsl.preload_tool_schemas(
                    ["rye.execute.tool.rye.primary.*"], tool_project,
                )

        assert result["schemas"] == ""
        assert result["preloaded_tools"] == []

    def test_non_tool_caps_ignored(self, tool_project):
        result = _tsl.preload_tool_schemas(
            ["rye.search.*", "rye.load.knowledge.foo"], tool_project,
        )
        assert result["schemas"] == ""
        assert result["preloaded_tools"] == []

    def test_empty_capabilities(self, tool_project):
        result = _tsl.preload_tool_schemas([], tool_project)
        assert result["schemas"] == ""
        assert result["preloaded_tools"] == []

    def test_token_budget_limits_output(self, tool_project):
        from unittest.mock import patch
        mock_paths = [(tool_project / ".ai" / "tools", "project")]
        with patch.object(_tsl.ToolResolver, "get_search_paths", return_value=mock_paths):
            with patch.object(_tsl, "get_tool_extensions", return_value=[".py"]):
                result = _tsl.preload_tool_schemas(
                    ["rye.execute.tool.rye.bash.*", "rye.execute.tool.rye.file-system.*"],
                    tool_project, max_tokens=10,
                )

        assert len(result["preloaded_tools"]) < 2

    def test_exact_tool_reference(self, tool_project):
        from unittest.mock import patch
        mock_paths = [(tool_project / ".ai" / "tools", "project")]
        with patch.object(_tsl.ToolResolver, "get_search_paths", return_value=mock_paths):
            with patch.object(_tsl, "get_tool_extensions", return_value=[".py"]):
                result = _tsl.preload_tool_schemas(
                    ["rye.execute.tool.rye.file-system.read"], tool_project,
                )

        assert result["preloaded_tools"] == ["rye/file-system/read"]

    def test_deduplicates_across_patterns(self, tool_project):
        """Same tool matched by wildcard and exact cap appears only once."""
        from unittest.mock import patch
        mock_paths = [(tool_project / ".ai" / "tools", "project")]
        with patch.object(_tsl.ToolResolver, "get_search_paths", return_value=mock_paths):
            with patch.object(_tsl, "get_tool_extensions", return_value=[".py"]):
                result = _tsl.preload_tool_schemas(
                    [
                        "rye.execute.tool.rye.bash.bash",
                        "rye.execute.tool.rye.bash.*",
                    ],
                    tool_project,
                )

        assert result["preloaded_tools"].count("rye/bash/bash") == 1


# ---------------------------------------------------------------------------
# Phase 2: resolve_extends hook conditions
# ---------------------------------------------------------------------------

class TestResolveExtendsConditions:
    def test_has_extends_false_matches(self):
        ctx = {"has_extends": False, "directive": "test/deploy"}
        condition = {"path": "has_extends", "op": "eq", "value": False}
        assert _ce.matches(ctx, condition)

    def test_has_extends_true_does_not_match_false_condition(self):
        ctx = {"has_extends": True, "directive": "test/deploy"}
        condition = {"path": "has_extends", "op": "eq", "value": False}
        assert not _ce.matches(ctx, condition)

    def test_has_extends_true_can_still_be_matched(self):
        """Hooks can match even when extends is already set (override)."""
        ctx = {"has_extends": True, "directive": "test/deploy"}
        condition = {"path": "directive", "op": "contains", "value": "deploy"}
        assert _ce.matches(ctx, condition)

    def test_directive_contains_condition(self):
        ctx = {"directive": "project/deploy/staging", "has_extends": False}
        condition = {"path": "directive", "op": "contains", "value": "deploy"}
        assert _ce.matches(ctx, condition)

        ctx["directive"] = "project/lint/check"
        assert not _ce.matches(ctx, condition)

    def test_empty_condition_always_matches(self):
        ctx = {"directive": "anything", "has_extends": False}
        assert _ce.matches(ctx, {})


# ---------------------------------------------------------------------------
# Phase 3: base directives and decomposed protocol
# ---------------------------------------------------------------------------

class TestBaseDirectives:
    def test_base_has_full_permissions_and_protocol(self):
        path = _STD_ROOT / ".ai" / "directives" / "rye" / "agent" / "core" / "base.md"
        assert path.exists()
        content = path.read_text()
        assert "<execute>*</execute>" in content
        assert "<search>*</search>" in content
        assert "<load>*</load>" in content
        assert "<sign>*</sign>" in content
        for primary in ("execute", "search", "load", "sign"):
            assert f"rye/agent/core/protocol/{primary}" in content
        assert "rye/agent/core/Identity" in content
        assert "rye/agent/core/Behavior" in content

    def test_base_execute_only_is_narrow(self):
        path = _STD_ROOT / ".ai" / "directives" / "rye" / "agent" / "core" / "base_execute_only.md"
        assert path.exists()
        content = path.read_text()
        assert "<execute>*</execute>" in content
        assert "<search>" not in content
        assert "<load>" not in content
        assert "<sign>" not in content
        assert "rye/agent/core/protocol/execute" in content
        assert "rye/agent/core/protocol/search" not in content

    def test_base_review_has_read_only_tools(self):
        path = _STD_ROOT / ".ai" / "directives" / "rye" / "agent" / "core" / "base_review.md"
        assert path.exists()
        content = path.read_text()
        assert "rye.file-system.read" in content
        assert "rye.file-system.glob" in content
        assert "rye.file-system.grep" in content
        assert "rye.bash" not in content


class TestDecomposedProtocol:
    def test_all_protocol_items_exist(self):
        proto_dir = _STD_ROOT / ".ai" / "knowledge" / "rye" / "agent" / "core" / "protocol"
        assert proto_dir.is_dir()
        for name in ("execute", "search", "load", "sign"):
            path = proto_dir / f"{name}.md"
            assert path.exists(), f"Missing protocol item: {name}"
            content = path.read_text()
            assert f"rye_{name}" in content

    def test_original_tool_protocol_still_exists(self):
        path = _STD_ROOT / ".ai" / "knowledge" / "rye" / "agent" / "core" / "ToolProtocol.md"
        assert path.exists()


# ---------------------------------------------------------------------------
# Phase 4: removed hooks
# ---------------------------------------------------------------------------

class TestHookConditionsPhase4:
    def test_removed_hooks_absent(self):
        hook_path = _STD_ROOT / ".ai" / "config" / "agent" / "hook_conditions.yaml"
        with open(hook_path) as f:
            config = yaml.safe_load(f)

        hook_ids = {h["id"] for h in config.get("context_hooks", [])}
        assert "system_identity" not in hook_ids
        assert "system_behavior" not in hook_ids
        assert "system_tool_protocol" not in hook_ids

    def test_kept_hooks_present(self):
        hook_path = _STD_ROOT / ".ai" / "config" / "agent" / "hook_conditions.yaml"
        with open(hook_path) as f:
            config = yaml.safe_load(f)

        hook_ids = {h["id"] for h in config.get("context_hooks", [])}
        assert "ctx_environment" in hook_ids
        assert "ctx_directive_instruction" in hook_ids

    def test_builtin_and_infra_hooks_unchanged(self):
        hook_path = _STD_ROOT / ".ai" / "config" / "agent" / "hook_conditions.yaml"
        with open(hook_path) as f:
            config = yaml.safe_load(f)

        builtin_ids = {h["id"] for h in config.get("builtin_hooks", [])}
        assert "default_retry_transient" in builtin_ids
        assert "default_fail_permanent" in builtin_ids
        assert "default_escalate_limit" in builtin_ids

        infra_ids = {h["id"] for h in config.get("infra_hooks", [])}
        assert "infra_save_state" in infra_ids
        assert "infra_completion_signal" in infra_ids
