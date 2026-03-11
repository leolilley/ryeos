"""Tests for execute tool."""

import asyncio
import importlib.util
import tempfile
from pathlib import Path

import pytest

from conftest import get_bundle_path
from rye.tools.execute import ExecuteTool

# Processors are data-driven core tools — load via bundle path
_INTERPOLATE_PATH = get_bundle_path(
    "core", "tools/rye/core/processors/inputs/interpolate.py"
)
_spec = importlib.util.spec_from_file_location("inputs_interpolate", _INTERPOLATE_PATH)
_interpolate_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_interpolate_mod)

_resolve_input_refs = _interpolate_mod._resolve_input_refs
_interpolate_parsed = _interpolate_mod._interpolate_parsed


@pytest.fixture
def temp_project(_setup_user_space):
    """Create temporary project with test items."""
    import os
    from rye.utils.trust_store import TrustStore
    
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        ai_dir = project_root / ".ai"

        # Create directive with proper markdown+xml format
        directives_dir = ai_dir / "directives"
        directives_dir.mkdir(parents=True)
        (directives_dir / "workflow.md").write_text('''# Workflow Directive

```xml
<directive name="workflow" version="1.0.0">
<process>
<step name="step1">Do something</step>
</process>
</directive>
```
''')

        # Create tool with proper metadata
        tools_dir = ai_dir / "tools"
        tools_dir.mkdir(parents=True)
        (tools_dir / "mytool.py").write_text('''
__version__ = "1.0.0"
__tool_type__ = "primitive"
__executor_id__ = None
__category__ = "test"

def main():
     print('tool')
''')

        # Create knowledge
        knowledge_dir = ai_dir / "knowledge"
        knowledge_dir.mkdir(parents=True)
        (knowledge_dir / "entry.md").write_text(
            "---\ntitle: Test Entry\nname: entry\n---\n\nContent here"
        )

        from rye.utils.metadata_manager import MetadataManager
        from rye.constants import ItemType, AI_DIR as RYE_AI_DIR

        # Get the signing public key from the setup fixture (already in user_space trust store)
        user_space = Path(os.environ.get("USER_SPACE"))
        signing_key_dir = user_space / RYE_AI_DIR / "config" / "keys" / "signing"
        from lillux.primitives.signing import load_keypair, compute_key_fingerprint
        _, public_pem_signing = load_keypair(signing_key_dir)
        signing_fp = compute_key_fingerprint(public_pem_signing)
        
        # Trust the signing key in this project so verification passes
        store = TrustStore(project_path=project_root)
        store.add_key(public_pem_signing, owner="local", space="project")

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
class TestExecuteTool:
    """Test execute tool."""

    async def test_execute_directive(self, temp_project):
        """Execute directive — returns parsed content in-thread by default."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="workflow",
            project_path=str(temp_project),
        )

        assert "your_directions" in result
        assert "metadata" in result

    async def test_execute_directive_threaded(self, temp_project):
        """Execute directive with thread="fork" — attempts to spawn thread.

        In a test environment without the full thread infrastructure,
        this errors because thread_directive tool can't be found.
        """
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="workflow",
            project_path=str(temp_project),
            thread="fork",
        )

        # thread_directive tool won't exist in the temp project
        assert "error" in result

    async def test_execute_tool(self, temp_project):
        """Execute tool - primitives without known type return error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
        )

        # The tool is a primitive with executor_id=None but not a known primitive
        # (subprocess, http_client), so it returns an error
        assert "error" in result

    async def test_execute_knowledge(self, temp_project):
        """Execute/load knowledge."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="knowledge",
            item_id="entry",
            project_path=str(temp_project),
        )

        # Knowledge execution returns with content and metadata
        assert "metadata" in result

    async def test_dry_run_directive(self, temp_project):
        """Dry run directive."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="workflow",
            project_path=str(temp_project),
            dry_run=True,
        )

        # Dry run returns validation results
        assert "metadata" in result or "error" not in result

    async def test_dry_run_tool(self, temp_project):
        """Dry run tool."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
            dry_run=True,
        )

        # Dry run validation (tool may error since it's unknown, but that's expected)
        assert isinstance(result, dict)

    async def test_execute_nonexistent_directive(self, temp_project):
        """Error on nonexistent directive."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="nonexistent",
            project_path=str(temp_project),
        )

        assert "error" in result

    async def test_execute_with_parameters(self, temp_project):
        """Execute with parameters - unknown primitive returns error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
            parameters={"arg1": "value1"},
        )

        # Unknown primitive returns error (mytool is not subprocess/http_client)
        assert "error" in result


@pytest.mark.asyncio
class TestAsyncValidation:
    """Test Step 5 validation: rejected async combinations."""

    async def test_async_dry_run_rejected(self, temp_project):
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
            dry_run=True,
            **{"async": True},
        )
        assert result["status"] == "error"
        assert "dry_run" in result["error"]

    async def test_async_knowledge_rejected(self, temp_project):
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="knowledge",
            item_id="entry",
            project_path=str(temp_project),
            **{"async": True},
        )
        assert result["status"] == "error"
        assert "knowledge" in result["error"]

    async def test_async_directive_inline_rejected(self, temp_project):
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="workflow",
            project_path=str(temp_project),
            thread="inline",
            **{"async": True},
        )
        assert result["status"] == "error"
        assert "inline" in result["error"]


@pytest.mark.asyncio
class TestThreadItemTypeValidation:
    """Test target/thread/item_type validation in handle() — rejects bad combos early."""

    async def test_tool_fork_rejected(self, temp_project):
        """tool + thread=fork → error (fork is for directives only)."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
            thread="fork",
        )
        assert result["status"] == "error"
        assert "fork" in result["error"]
        assert result["item_id"] == "mytool"

    async def test_knowledge_remote_rejected(self, temp_project):
        """knowledge + target=remote → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="knowledge",
            item_id="entry",
            project_path=str(temp_project),
            target="remote",
        )
        assert result["status"] == "error"
        assert "knowledge" in result["error"].lower()

    async def test_knowledge_fork_rejected(self, temp_project):
        """knowledge + thread=fork → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="knowledge",
            item_id="entry",
            project_path=str(temp_project),
            thread="fork",
        )
        assert result["status"] == "error"
        assert "knowledge" in result["error"].lower()

    async def test_unknown_thread_rejected(self, temp_project):
        """Unknown thread value → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
            thread="banana",
        )
        assert result["status"] == "error"
        assert "banana" in result["error"]

    async def test_unknown_target_rejected(self, temp_project):
        """Unknown target value → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
            target="banana",
        )
        assert result["status"] == "error"
        assert "banana" in result["error"]

    async def test_remote_empty_suffix_rejected(self, temp_project):
        """target='remote:' with empty suffix → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
            target="remote:",
        )
        assert result["status"] == "error"
        assert "empty" in result["error"].lower()

    async def test_dry_run_remote_rejected(self, temp_project):
        """dry_run + target=remote → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
            target="remote",
            dry_run=True,
        )
        assert result["status"] == "error"
        assert "dry_run" in result["error"].lower() or "remote" in result["error"].lower()

    async def test_tool_inline_allowed(self, temp_project):
        """tool + thread=inline → not rejected by validation (may fail later)."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="tool",
            item_id="mytool",
            project_path=str(temp_project),
            thread="inline",
        )
        if result.get("status") == "error":
            assert "fork" not in result.get("error", "")
            assert "not supported" not in result.get("error", "").lower()

    async def test_directive_inline_allowed(self, temp_project):
        """directive + thread=inline → allowed, returns your_directions."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="directive",
            item_id="workflow",
            project_path=str(temp_project),
            thread="inline",
        )
        assert "your_directions" in result

    async def test_knowledge_inline_allowed(self, temp_project):
        """knowledge + thread=inline (default) → allowed."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_type="knowledge",
            item_id="entry",
            project_path=str(temp_project),
            thread="inline",
        )
        if result.get("status") == "error":
            assert "not supported" not in result.get("error", "").lower()


@pytest.mark.asyncio
class TestRemoteThreadForwarding:
    """Verify remote target sets correct thread in params for the remote tool."""

    async def test_directive_remote_forwards_fork(self, temp_project):
        """Directive + target=remote → remote_params.thread = 'fork'."""
        from unittest.mock import AsyncMock, patch

        tool = ExecuteTool("")

        original_find = tool._find_item
        def mock_find(pp, it, iid):
            if iid == "rye/core/remote/remote":
                return Path("/fake/remote.py")
            return original_find(pp, it, iid)

        captured_params = {}
        async def mock_run_tool(item_id, project_path, parameters, dry_run, **kw):
            if item_id == "rye/core/remote/remote":
                captured_params.update(parameters)
                return {"status": "success", "data": {"status": "success", "thread_id": "t-123"}}
            return {"status": "error", "error": "unexpected"}

        with patch.object(tool, "_find_item", side_effect=mock_find), \
             patch.object(tool, "_run_tool", side_effect=mock_run_tool):
            await tool.handle(
                item_type="directive",
                item_id="workflow",
                project_path=str(temp_project),
                target="remote",
                thread="fork",
            )

        assert captured_params.get("thread") == "fork"
        assert captured_params.get("action") == "execute"
        assert captured_params.get("item_type") == "directive"

    async def test_tool_remote_forwards_inline(self, temp_project):
        """Tool + target=remote → remote_params.thread = 'inline'."""
        from unittest.mock import AsyncMock, patch

        tool = ExecuteTool("")

        original_find = tool._find_item
        def mock_find(pp, it, iid):
            if iid == "rye/core/remote/remote":
                return Path("/fake/remote.py")
            return original_find(pp, it, iid)

        captured_params = {}
        async def mock_run_tool(item_id, project_path, parameters, dry_run, **kw):
            if item_id == "rye/core/remote/remote":
                captured_params.update(parameters)
                return {"status": "success", "data": {"status": "success", "result": {}}}
            return {"status": "error", "error": "unexpected"}

        with patch.object(tool, "_find_item", side_effect=mock_find), \
             patch.object(tool, "_run_tool", side_effect=mock_run_tool):
            await tool.handle(
                item_type="tool",
                item_id="mytool",
                project_path=str(temp_project),
                target="remote",
                thread="inline",
            )

        assert captured_params.get("thread") == "inline"
        assert captured_params.get("action") == "execute"
        assert captured_params.get("item_type") == "tool"

    async def test_directive_remote_named_forwards_remote_name(self, temp_project):
        """Directive + target=remote:gpu → remote_params has remote='gpu'."""
        from unittest.mock import patch

        tool = ExecuteTool("")

        original_find = tool._find_item
        def mock_find(pp, it, iid):
            if iid == "rye/core/remote/remote":
                return Path("/fake/remote.py")
            return original_find(pp, it, iid)

        captured_params = {}
        async def mock_run_tool(item_id, project_path, parameters, dry_run, **kw):
            if item_id == "rye/core/remote/remote":
                captured_params.update(parameters)
                return {"status": "success", "data": {"status": "success", "thread_id": "t-456"}}
            return {"status": "error", "error": "unexpected"}

        with patch.object(tool, "_find_item", side_effect=mock_find), \
             patch.object(tool, "_run_tool", side_effect=mock_run_tool):
            await tool.handle(
                item_type="directive",
                item_id="workflow",
                project_path=str(temp_project),
                target="remote:gpu",
                thread="fork",
            )

        assert captured_params.get("thread") == "fork"
        assert captured_params.get("remote") == "gpu"


@pytest.mark.asyncio
class TestLaunchAsync:
    """Test _launch_async spawns detached process and returns handle."""

    async def test_tool_async_no_registry_uses_launch_detached(self, temp_project):
        """Without registry, falls back to raw launch_detached."""
        from unittest.mock import AsyncMock, patch

        mock_spawn = AsyncMock(return_value={"success": True, "pid": 9999})
        tool = ExecuteTool("")

        with patch("rye.utils.detached.launch_detached", mock_spawn):
            result = await tool.handle(
                item_type="tool",
                item_id="mytool",
                project_path=str(temp_project),
                **{"async": True},
            )

        assert result["status"] == "success"
        assert result["async"] is True
        assert result["pid"] == 9999
        assert result["type"] == "tool"
        assert result["item_id"] == "mytool"
        assert result["state"] == "running"
        assert "thread_id" in result

    async def test_tool_async_with_registry_uses_spawn_thread(self, temp_project):
        """With registry available, uses spawn_thread for proper lifecycle."""
        from unittest.mock import AsyncMock, MagicMock, patch

        mock_registry = MagicMock()
        mock_spawn = AsyncMock(return_value={"success": True, "pid": 42})
        tool = ExecuteTool("")

        with patch.object(ExecuteTool, "_get_registry", return_value=mock_registry), \
             patch("rye.utils.detached.spawn_thread", mock_spawn) as mock_st:
            result = await tool.handle(
                item_type="tool",
                item_id="mytool",
                project_path=str(temp_project),
                **{"async": True},
            )

        assert result["status"] == "success"
        assert result["pid"] == 42
        # spawn_thread was called with the registry
        mock_st.assert_awaited_once()
        call_kwargs = mock_st.call_args.kwargs
        assert call_kwargs["registry"] is mock_registry
        assert call_kwargs["directive"] == "tool/mytool"
        assert "thread_id" in call_kwargs

    async def test_tool_async_spawn_failure(self, temp_project):
        from unittest.mock import AsyncMock, patch

        mock_spawn = AsyncMock(return_value={"success": False, "error": "no lillux-proc"})
        tool = ExecuteTool("")

        with patch("rye.utils.detached.launch_detached", mock_spawn):
            result = await tool.handle(
                item_type="tool",
                item_id="mytool",
                project_path=str(temp_project),
                **{"async": True},
            )

        assert result["status"] == "error"
        assert "spawn" in result["error"]


class TestResolveInputRefs:
    """Unit tests for {input:key} interpolation."""

    def test_basic_resolve(self):
        assert _resolve_input_refs("{input:name}", {"name": "alice"}) == "alice"

    def test_missing_kept_as_is(self):
        assert _resolve_input_refs("{input:name}", {}) == "{input:name}"

    def test_optional_missing_empty(self):
        assert _resolve_input_refs("{input:name?}", {}) == ""

    def test_optional_present_resolves(self):
        assert _resolve_input_refs("{input:name?}", {"name": "alice"}) == "alice"

    def test_default_missing_uses_fallback(self):
        assert _resolve_input_refs("{input:mode:verbose}", {}) == "verbose"

    def test_default_present_resolves(self):
        assert _resolve_input_refs("{input:mode:verbose}", {"mode": "quiet"}) == "quiet"

    def test_mixed_in_sentence(self):
        result = _resolve_input_refs(
            "Write {input:topic} to {input:path} ({input:mode:overview}){input:suffix?}",
            {"topic": "rust", "path": "/tmp/out"},
        )
        assert result == "Write rust to /tmp/out (overview)"

    def test_no_placeholders_passthrough(self):
        assert _resolve_input_refs("plain text", {"x": "y"}) == "plain text"


class TestInterpolateParsed:
    """Unit tests for _interpolate_parsed on directive data dicts."""

    def test_interpolates_body(self):
        parsed = {"body": "Research {input:topic}"}
        _interpolate_parsed(parsed, {"topic": "rust"})
        assert parsed["body"] == "Research rust"

    def test_interpolates_action_params(self):
        parsed = {
            "actions": [
                {
                    "primary": "execute",
                    "item_type": "tool",
                    "item_id": "fs_write",
                    "params": {"path": "{input:out}", "content": "{input:data?}"},
                }
            ]
        }
        _interpolate_parsed(parsed, {"out": "/tmp/x"})
        assert parsed["actions"][0]["params"]["path"] == "/tmp/x"
        assert parsed["actions"][0]["params"]["content"] == ""

    def test_interpolates_action_attributes(self):
        parsed = {
            "actions": [
                {"primary": "search", "query": "{input:q}", "item_type": "knowledge"}
            ]
        }
        _interpolate_parsed(parsed, {"q": "patterns"})
        assert parsed["actions"][0]["query"] == "patterns"

    def test_no_actions_no_error(self):
        parsed = {"body": "hello"}
        _interpolate_parsed(parsed, {"x": "y"})
        assert parsed["body"] == "hello"


class TestParseTarget:
    """Tests for ExecuteTool._parse_target()."""

    def test_local(self):
        assert ExecuteTool._parse_target("local") == ("local", None)

    def test_remote_default(self):
        assert ExecuteTool._parse_target("remote") == ("remote", None)

    def test_remote_named(self):
        assert ExecuteTool._parse_target("remote:gpu") == ("remote", "gpu")

    def test_remote_named_with_dashes(self):
        assert ExecuteTool._parse_target("remote:my-gpu-server") == ("remote", "my-gpu-server")

    def test_remote_empty_suffix_raises(self):
        with pytest.raises(ValueError, match="empty"):
            ExecuteTool._parse_target("remote:")

    def test_unknown_target_raises(self):
        with pytest.raises(ValueError, match="Unknown target"):
            ExecuteTool._parse_target("banana")
