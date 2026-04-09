"""Tests for execute tool — kind-agnostic engine.

The engine dispatches based on executor_id from the extractor system:
- @primitive_chain → PrimitiveExecutor (self-executing tools)
- any other value → executor tool dispatch (directives, etc.)
"""

import asyncio
import importlib.util
import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from conftest import get_bundle_path
from rye.actions.execute import ExecuteTool, PRIMITIVE_CHAIN

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
    from rye.utils.execution_context import ExecutionContext

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

        # Create knowledge (no executor_id — will error on execution)
        knowledge_dir = ai_dir / "knowledge"
        knowledge_dir.mkdir(parents=True)
        (knowledge_dir / "entry.md").write_text(
            "---\ntitle: Test Entry\nname: entry\n---\n\nContent here"
        )

        from rye.utils.metadata_manager import MetadataManager
        from rye.constants import ItemType, AI_DIR as RYE_AI_DIR

        user_space = Path(os.environ.get("USER_SPACE"))
        signing_key_dir = user_space / RYE_AI_DIR / "config" / "keys" / "signing"
        from rye.primitives.signing import load_keypair, compute_key_fingerprint
        _, public_pem_signing = load_keypair(signing_key_dir)

        store = TrustStore(ExecutionContext.from_env(project_path=project_root))
        store.add_key(public_pem_signing, owner="local", space="project", version="1.0.0")

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
    """Test engine dispatch behaviour."""

    async def test_execute_directive_dispatches_to_executor(self, temp_project):
        """Directive dispatches to its executor tool (thread_directive)."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="directive:workflow",
            project_path=str(temp_project),
        )
        # The engine dispatches to thread_directive executor tool.
        # In test env, the executor may succeed or error — but it MUST NOT
        # return your_directions (that was the old inline optimization).
        assert "your_directions" not in result
        assert isinstance(result, dict)

    async def test_execute_tool(self, temp_project):
        """Tool dispatches via @primitive_chain (PrimitiveExecutor)."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:mytool",
            project_path=str(temp_project),
        )
        # mytool is a primitive with unknown type, so execution errors
        assert "error" in result

    async def test_execute_knowledge_dispatches_to_executor(self, temp_project):
        """Knowledge dispatches to its executor tool."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="knowledge:entry",
            project_path=str(temp_project),
        )
        # Executor tool may not be in test project — but it must NOT
        # error on missing executor_id (extractor provides it).
        assert isinstance(result, dict)

    async def test_dry_run_tool(self, temp_project):
        """Dry run tool via @primitive_chain."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:mytool",
            project_path=str(temp_project),
            dry_run=True,
        )
        assert isinstance(result, dict)

    async def test_execute_nonexistent_directive(self, temp_project):
        """Error on nonexistent directive."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="directive:nonexistent",
            project_path=str(temp_project),
        )
        assert "error" in result

    async def test_execute_with_parameters(self, temp_project):
        """Execute with parameters — unknown primitive returns error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:mytool",
            project_path=str(temp_project),
            parameters={"arg1": "value1"},
        )
        assert "error" in result


@pytest.mark.asyncio
class TestCanonicalRefResolution:
    """Test canonical ref resolution in execute."""

    async def test_bare_directive_rejected(self, temp_project):
        """Bare ID is rejected — canonical ref required."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="workflow",
            project_path=str(temp_project),
        )
        assert result["status"] == "error"
        assert "canonical" in result["error"].lower()

    async def test_bare_tool_rejected(self, temp_project):
        """Bare ID is rejected — canonical ref required."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="mytool",
            project_path=str(temp_project),
        )
        assert result["status"] == "error"
        assert "canonical" in result["error"].lower()

    async def test_canonical_tool_ref(self, temp_project):
        """Canonical tool:id ref works."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:mytool",
            project_path=str(temp_project),
        )
        assert isinstance(result, dict)

    async def test_canonical_directive_ref(self, temp_project):
        """Canonical directive:id ref dispatches to executor."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="directive:workflow",
            project_path=str(temp_project),
        )
        assert isinstance(result, dict)
        assert "your_directions" not in result  # no inline optimization

    async def test_knowledge_dispatches_to_executor(self, temp_project):
        """knowledge: ref resolves executor_id from extractor."""
        tool = ExecuteTool("")
        resolved = tool._resolve_executable_ref(str(temp_project), "knowledge:entry")
        assert resolved.executor_id == "rye/core/executors/knowledge/knowledge"

    async def test_nonexistent_canonical_ref(self, temp_project):
        """Canonical ref to nonexistent item returns not-found error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:doesnotexist",
            project_path=str(temp_project),
        )
        assert result["status"] == "error"
        assert "not found" in result["error"].lower()

    async def test_empty_canonical_ref_rejected(self, temp_project):
        """Empty canonical ref (e.g. 'tool:') is rejected."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:",
            project_path=str(temp_project),
        )
        assert result["status"] == "error"


@pytest.mark.asyncio
class TestProtocolValidation:
    """Test protocol-level validation (no kind checks)."""

    async def test_async_dry_run_rejected(self, temp_project):
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:mytool",
            project_path=str(temp_project),
            dry_run=True,
            **{"async": True},
        )
        assert result["status"] == "error"
        assert "dry_run" in result["error"]

    async def test_unknown_thread_rejected(self, temp_project):
        """Unknown thread value → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:mytool",
            project_path=str(temp_project),
            thread="banana",
        )
        assert result["status"] == "error"
        assert "banana" in result["error"]

    async def test_unknown_target_rejected(self, temp_project):
        """Unknown target value → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:mytool",
            project_path=str(temp_project),
            target="banana",
        )
        assert result["status"] == "error"
        assert "banana" in result["error"]

    async def test_remote_empty_suffix_rejected(self, temp_project):
        """target='remote:' with empty suffix → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:mytool",
            project_path=str(temp_project),
            target="remote:",
        )
        assert result["status"] == "error"
        assert "empty" in result["error"].lower()

    async def test_dry_run_remote_rejected(self, temp_project):
        """dry_run + target=remote → error."""
        tool = ExecuteTool("")
        result = await tool.handle(
            item_id="tool:mytool",
            project_path=str(temp_project),
            target="remote",
            dry_run=True,
        )
        assert result["status"] == "error"


@pytest.mark.asyncio
class TestDispatchStrategy:
    """Test that dispatch is based on executor_id, not kind."""

    async def test_tool_dispatches_via_primitive_chain(self, temp_project):
        """Tool has executor_id=@primitive_chain → PrimitiveExecutor."""
        tool = ExecuteTool("")

        # Resolve the executable and check executor_id
        resolved = tool._resolve_executable_ref(str(temp_project), "tool:mytool")
        assert resolved.executor_id == PRIMITIVE_CHAIN

    async def test_directive_dispatches_via_executor_tool(self, temp_project):
        """Directive has executor_id from extractor (not @primitive_chain)."""
        tool = ExecuteTool("")

        resolved = tool._resolve_executable_ref(str(temp_project), "directive:workflow")
        assert resolved.executor_id == "rye/core/executors/directive/directive"
        assert resolved.executor_id != PRIMITIVE_CHAIN

    async def test_all_kinds_have_executor_id(self, temp_project):
        """All standard kinds resolve executor_id from extractors."""
        tool = ExecuteTool("")

        tool_resolved = tool._resolve_executable_ref(str(temp_project), "tool:mytool")
        assert tool_resolved.executor_id == PRIMITIVE_CHAIN

        directive_resolved = tool._resolve_executable_ref(str(temp_project), "directive:workflow")
        assert directive_resolved.executor_id == "rye/core/executors/directive/directive"

        knowledge_resolved = tool._resolve_executable_ref(str(temp_project), "knowledge:entry")
        assert knowledge_resolved.executor_id == "rye/core/executors/knowledge/knowledge"


@pytest.mark.asyncio
class TestRemoteThreadForwarding:
    """Verify remote target forwards canonical ref uniformly."""

    async def test_directive_remote_forwards(self, temp_project):
        """Directive + target=remote → forwards canonical ref to remote tool."""
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
                item_id="directive:workflow",
                project_path=str(temp_project),
                target="remote",
                thread="fork",
            )

        assert captured_params.get("action") == "execute"
        assert captured_params.get("item_id") == "directive:workflow"
        assert captured_params.get("thread") == "fork"

    async def test_tool_remote_forwards(self, temp_project):
        """Tool + target=remote → forwards canonical ref to remote tool."""
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
                item_id="tool:mytool",
                project_path=str(temp_project),
                target="remote",
                thread="inline",
            )

        assert captured_params.get("action") == "execute"
        assert captured_params.get("item_id") == "tool:mytool"
        assert captured_params.get("thread") == "inline"

    async def test_remote_named_forwards_remote_name(self, temp_project):
        """target=remote:gpu → remote_params has remote='gpu'."""
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
                item_id="directive:workflow",
                project_path=str(temp_project),
                target="remote:gpu",
                thread="fork",
            )

        assert captured_params.get("remote") == "gpu"


@pytest.mark.asyncio
class TestLaunchAsync:
    """Test _launch_async spawns detached process and returns handle."""

    async def test_tool_async_no_registry_uses_launch_detached(self, temp_project):
        """Without registry, falls back to raw launch_detached."""
        mock_spawn = AsyncMock(return_value={"success": True, "pid": 9999})
        tool = ExecuteTool("")

        with patch("rye.utils.detached.launch_detached", mock_spawn):
            result = await tool.handle(
                item_id="tool:mytool",
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
        mock_registry = MagicMock()
        mock_spawn = AsyncMock(return_value={"success": True, "pid": 42})
        tool = ExecuteTool("")

        with patch.object(ExecuteTool, "_get_registry", return_value=mock_registry), \
             patch("rye.utils.detached.spawn_thread", mock_spawn) as mock_st:
            result = await tool.handle(
                item_id="tool:mytool",
                project_path=str(temp_project),
                **{"async": True},
            )

        assert result["status"] == "success"
        assert result["pid"] == 42
        mock_st.assert_awaited_once()
        call_kwargs = mock_st.call_args.kwargs
        assert call_kwargs["registry"] is mock_registry
        assert call_kwargs["item_id"] == "tool:mytool"
        assert "thread_id" in call_kwargs

    async def test_tool_async_spawn_failure(self, temp_project):
        mock_spawn = AsyncMock(return_value={"success": False, "error": "no lillux"})
        tool = ExecuteTool("")

        with patch("rye.utils.detached.launch_detached", mock_spawn):
            result = await tool.handle(
                item_id="tool:mytool",
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
                {"primary": "fetch", "query": "{input:q}", "item_type": "knowledge"}
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


# ---------------------------------------------------------------------------
# Contract tests — execution ownership (for @primitive_chain items)
# ---------------------------------------------------------------------------


class TestExecutionSpec:
    """Test _read_execution_spec and _resolve_execution_plan contracts."""

    def test_spec_omission_defaults_to_engine(self):
        """Default spec is engine-owned."""
        from rye.actions.execute import ExecutionSpec

        spec = ExecutionSpec()
        assert spec.owner == "engine"
        assert spec.native_async is False
        assert spec.native_resume is False

    def test_plan_remote_always_forward(self):
        """Remote target always produces forward_remote."""
        from rye.actions.execute import ExecutionSpec

        callee_spec = ExecutionSpec(owner="callee", native_async=True)
        plan = ExecuteTool._resolve_execution_plan(
            "remote", False, callee_spec,
        )
        assert plan.owner == "remote"
        assert plan.launch_mode == "forward_remote"

    def test_plan_callee_owned_direct(self):
        """Callee-owned spec produces direct launch."""
        from rye.actions.execute import ExecutionSpec

        spec = ExecutionSpec(owner="callee", native_async=True, native_resume=True)
        plan = ExecuteTool._resolve_execution_plan(
            "local", True, spec,
        )
        assert plan.owner == "callee"
        assert plan.launch_mode == "direct"
        assert plan.native_async is True
        assert plan.native_resume is True

    def test_plan_engine_owned_async_detach(self):
        """Engine-owned + async produces engine_detach."""
        from rye.actions.execute import ExecutionSpec

        spec = ExecutionSpec()  # engine-owned
        plan = ExecuteTool._resolve_execution_plan(
            "local", True, spec,
        )
        assert plan.owner == "engine"
        assert plan.launch_mode == "engine_detach"

    def test_plan_engine_owned_sync_direct(self):
        """Engine-owned + sync produces direct."""
        from rye.actions.execute import ExecutionSpec

        spec = ExecutionSpec()
        plan = ExecuteTool._resolve_execution_plan(
            "local", False, spec,
        )
        assert plan.owner == "engine"
        assert plan.launch_mode == "direct"


@pytest.mark.asyncio
class TestExecutionOwnershipContracts:
    """Contract tests: callee-owned tools never go through _launch_async."""

    async def test_callee_owned_async_does_not_launch_async(self, temp_project):
        """Callee-owned tool with async=True must NOT call _launch_async."""
        tool = ExecuteTool("")

        tools_dir = temp_project / ".ai" / "tools"
        (tools_dir / "callee_tool.py").write_text('''
__version__ = "1.0.0"
__tool_type__ = "primitive"
__executor_id__ = None
__category__ = "test"
__execution_owner__ = "callee"
__native_async__ = True
__native_resume__ = True

def main():
    print("callee")
''')
        from rye.utils.metadata_manager import MetadataManager
        from rye.constants import ItemType
        content = (tools_dir / "callee_tool.py").read_text()
        signed = MetadataManager.sign_content(
            ItemType.TOOL, content,
            file_path=tools_dir / "callee_tool.py",
            project_path=temp_project,
        )
        (tools_dir / "callee_tool.py").write_text(signed)

        spec = await tool._read_execution_spec("callee_tool", str(temp_project))
        assert spec.owner == "callee"
        assert spec.native_async is True

        plan = ExecuteTool._resolve_execution_plan("local", True, spec)
        assert plan.launch_mode == "direct"
        assert plan.owner == "callee"

    async def test_engine_owned_async_uses_engine_detach(self, temp_project):
        """Engine-owned tool with async=True produces engine_detach."""
        tool = ExecuteTool("")

        spec = await tool._read_execution_spec("mytool", str(temp_project))
        assert spec.owner == "engine"
        assert spec.native_async is False

        plan = ExecuteTool._resolve_execution_plan("local", True, spec)
        assert plan.launch_mode == "engine_detach"
        assert plan.owner == "engine"

    async def test_graph_spec_from_chain(self, temp_project):
        """Tool resolving through a runtime with execution dunders reads spec."""
        from rye.utils.metadata_manager import MetadataManager
        from rye.constants import ItemType

        tools_dir = temp_project / ".ai" / "tools"

        runtime_dir = tools_dir / "test_runtime"
        runtime_dir.mkdir(parents=True)

        (runtime_dir / "runtime.yaml").write_text('''version: "1.0.0"
tool_type: runtime
executor_id: null
category: test_runtime
execution_owner: callee
native_async: true
native_resume: true
config:
  command: echo
  args: ["hello"]
''')
        content = (runtime_dir / "runtime.yaml").read_text()
        signed = MetadataManager.sign_content(
            ItemType.TOOL, content,
            file_path=runtime_dir / "runtime.yaml",
            project_path=temp_project,
        )
        (runtime_dir / "runtime.yaml").write_text(signed)

        (tools_dir / "graph_like.yaml").write_text('''version: "1.0.0"
tool_type: graph
executor_id: test_runtime/runtime
category: test
config:
  start: node1
  nodes:
    node1:
      action: echo
''')
        content = (tools_dir / "graph_like.yaml").read_text()
        signed = MetadataManager.sign_content(
            ItemType.TOOL, content,
            file_path=tools_dir / "graph_like.yaml",
            project_path=temp_project,
        )
        (tools_dir / "graph_like.yaml").write_text(signed)

        tool = ExecuteTool("")
        spec = await tool._read_execution_spec("graph_like", str(temp_project))
        assert spec.owner == "callee"
        assert spec.native_async is True
        assert spec.native_resume is True

    async def test_remote_forward_async(self):
        """Remote always produces forward_remote."""
        from rye.actions.execute import ExecutionSpec

        spec = ExecutionSpec(owner="callee", native_async=True)
        plan = ExecuteTool._resolve_execution_plan("remote", True, spec)
        assert plan.launch_mode == "forward_remote"
        assert plan.owner == "remote"

    async def test_spec_omission_defaults_engine(self, temp_project):
        """Tool without execution dunders defaults to engine-owned."""
        tool = ExecuteTool("")
        spec = await tool._read_execution_spec("mytool", str(temp_project))
        assert spec.owner == "engine"
        assert spec.native_async is False
        assert spec.native_resume is False

    async def test_chain_first_declarer_wins(self, temp_project):
        """First element in chain that declares ownership wins."""
        from rye.utils.metadata_manager import MetadataManager
        from rye.constants import ItemType

        tools_dir = temp_project / ".ai" / "tools"

        rt_dir = tools_dir / "plain_rt"
        rt_dir.mkdir(parents=True)
        (rt_dir / "runtime.yaml").write_text('''version: "1.0.0"
tool_type: runtime
executor_id: null
category: plain_rt
config:
  command: echo
  args: ["hello"]
''')
        content = (rt_dir / "runtime.yaml").read_text()
        signed = MetadataManager.sign_content(
            ItemType.TOOL, content,
            file_path=rt_dir / "runtime.yaml",
            project_path=temp_project,
        )
        (rt_dir / "runtime.yaml").write_text(signed)

        (tools_dir / "leaf_callee.py").write_text('''
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "plain_rt/runtime"
__category__ = "test"
__execution_owner__ = "callee"
__native_async__ = True
''')
        content = (tools_dir / "leaf_callee.py").read_text()
        signed = MetadataManager.sign_content(
            ItemType.TOOL, content,
            file_path=tools_dir / "leaf_callee.py",
            project_path=temp_project,
        )
        (tools_dir / "leaf_callee.py").write_text(signed)

        tool = ExecuteTool("")
        spec = await tool._read_execution_spec("leaf_callee", str(temp_project))
        assert spec.owner == "callee"
        assert spec.native_async is True
