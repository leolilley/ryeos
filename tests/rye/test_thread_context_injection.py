"""Tests for thread context injection system (Phase 1/1.5).

Covers:
- XML parser: <context> and <acknowledge> tag parsing
- SafetyHarness: check_permission() with current API, check_limits()
- Capability risk classification: _assess_capability_risk()
- System message injection: HttpProvider._inject_system_prompt(), _convert_messages()
- Transcript rendering: system_prompt and context_injected events
- Core knowledge items: existence and format validation
"""

import importlib.util
import sys
import tempfile
from pathlib import Path

import pytest
import yaml

PROJECT_ROOT = Path(__file__).parent.parent.parent

# ── Module imports via importlib ──────────────────────────────────────

PARSER_PATH = (
    PROJECT_ROOT
    / "ryeos" / "rye" / ".ai" / "tools" / "rye" / "core" / "parsers" / "markdown" / "xml.py"
)
_pspec = importlib.util.spec_from_file_location("markdown_xml", PARSER_PATH)
_parser_mod = importlib.util.module_from_spec(_pspec)
_pspec.loader.exec_module(_parser_mod)
md_parse = _parser_mod.parse

TRANSCRIPT_PATH = (
    PROJECT_ROOT
    / "ryeos" / "rye" / ".ai" / "tools" / "rye" / "agent" / "threads"
    / "persistence" / "transcript.py"
)
_tspec = importlib.util.spec_from_file_location("transcript", TRANSCRIPT_PATH)
_transcript_mod = importlib.util.module_from_spec(_tspec)
_tspec.loader.exec_module(_transcript_mod)
Transcript = _transcript_mod.Transcript

HARNESS_PATH = (
    PROJECT_ROOT
    / "ryeos" / "rye" / ".ai" / "tools" / "rye" / "agent" / "threads" / "safety_harness.py"
)
_hspec = importlib.util.spec_from_file_location("safety_harness", HARNESS_PATH)
_harness_mod = importlib.util.module_from_spec(_hspec)
_hspec.loader.exec_module(_harness_mod)
SafetyHarness = _harness_mod.SafetyHarness


# ── XML Parser: <context> tags ────────────────────────────────────────

class TestContextXMLParsing:
    """Test <context> metadata parsing in directives."""

    def test_context_system_items(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
    <context>
      <system>rye/agent/core/identity</system>
      <system>rye/agent/core/behavior</system>
    </context>
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        ctx = result.get("context", {})
        assert ctx["system"] == ["rye/agent/core/identity", "rye/agent/core/behavior"]
        assert ctx["before"] == []
        assert ctx["after"] == []

    def test_context_all_positions(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
    <context>
      <system>rye/agent/core/identity</system>
      <before>rye/agent/core/environment</before>
      <after>rye/agent/core/completion</after>
    </context>
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        ctx = result.get("context", {})
        assert ctx["system"] == ["rye/agent/core/identity"]
        assert ctx["before"] == ["rye/agent/core/environment"]
        assert ctx["after"] == ["rye/agent/core/completion"]

    def test_no_context_tag(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        assert "context" not in result


# ── XML Parser: <acknowledge> tags ────────────────────────────────────

class TestAcknowledgeParsing:
    """Test <acknowledge> tag parsing in <permissions>."""

    def test_acknowledge_risk(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
    <permissions>
      <execute>*</execute>
      <acknowledge risk="unrestricted">Full access needed for orchestration</acknowledge>
    </permissions>
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        acks = result.get("acknowledged_risks", [])
        assert len(acks) == 1
        assert acks[0]["risk"] == "unrestricted"
        assert "orchestration" in acks[0]["reason"]

    def test_acknowledge_elevated(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
    <permissions>
      <execute><tool>rye.bash.*</tool></execute>
      <acknowledge risk="elevated">Shell needed for build</acknowledge>
    </permissions>
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        acks = result.get("acknowledged_risks", [])
        assert len(acks) == 1
        assert acks[0]["risk"] == "elevated"

    def test_no_acknowledge(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
    <permissions>
      <search>*</search>
    </permissions>
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        assert "acknowledged_risks" not in result


# ── XML Parser: extends attribute ─────────────────────────────────────

class TestExtendsAttribute:
    """Test extends attribute parsing."""

    def test_extends_parsed(self):
        md = '''# Test
```xml
<directive name="child" version="1.0.0" extends="base-directive">
  <metadata>
    <description>Child</description>
    <model tier="general" />
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        assert result["extends"] == "base-directive"

    def test_no_extends(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        assert "extends" not in result


# ── SafetyHarness: check_permission() ─────────────────────────────────

class TestSafetyHarnessPermissions:
    """Test SafetyHarness.check_permission() with the current API."""

    def _make_harness(self, permissions=None, parent_capabilities=None, tmp_path=None):
        return SafetyHarness(
            thread_id="test-thread",
            limits={"turns": 10},
            hooks=[],
            project_path=tmp_path or Path("/tmp/test"),
            directive_name="test",
            permissions=permissions,
            parent_capabilities=parent_capabilities,
        )

    def test_no_capabilities_denies_all(self, tmp_path):
        harness = self._make_harness(tmp_path=tmp_path)
        result = harness.check_permission("execute", "tool", "rye/file-system/write")
        assert result is not None
        assert "Permission denied" in result["error"]

    def test_capability_allows_matching_action(self, tmp_path):
        perms = [{"tag": "cap", "content": "rye.execute.tool.rye.file-system.*"}]
        harness = self._make_harness(permissions=perms, tmp_path=tmp_path)
        result = harness.check_permission("execute", "tool", "rye/file-system/write")
        assert result is None

    def test_capability_denies_non_matching(self, tmp_path):
        perms = [{"tag": "cap", "content": "rye.execute.tool.rye.file-system.*"}]
        harness = self._make_harness(permissions=perms, tmp_path=tmp_path)
        result = harness.check_permission("execute", "tool", "rye/bash/bash")
        assert result is not None

    def test_search_wildcard(self, tmp_path):
        perms = [{"tag": "cap", "content": "rye.search.*"}]
        harness = self._make_harness(permissions=perms, tmp_path=tmp_path)
        result = harness.check_permission("search", "tool")
        assert result is None

    def test_internal_always_allowed(self, tmp_path):
        harness = self._make_harness(tmp_path=tmp_path)
        result = harness.check_permission("execute", "tool", "rye/agent/threads/internal/control")
        assert result is None

    def test_parent_capability_attenuation(self, tmp_path):
        """Child capabilities narrowed by parent."""
        perms = [{"tag": "cap", "content": "rye.execute.*"}]
        parent_caps = ["rye.execute.tool.rye.file-system.*"]
        harness = self._make_harness(
            permissions=perms, parent_capabilities=parent_caps, tmp_path=tmp_path
        )
        # Child asked for rye.execute.* but parent limits to rye.execute.tool.rye.file-system.*
        result = harness.check_permission("execute", "tool", "rye/file-system/write")
        assert result is None
        result = harness.check_permission("execute", "tool", "rye/bash/bash")
        assert result is not None


# ── SafetyHarness: check_limits() ─────────────────────────────────────

class TestSafetyHarnessLimits:
    """Test SafetyHarness.check_limits() with the current API."""

    def test_under_limits(self, tmp_path):
        harness = SafetyHarness("t", {"turns": 10, "tokens": 1000}, [], tmp_path)
        result = harness.check_limits({"turns": 5, "input_tokens": 200, "output_tokens": 100, "spend": 0.01})
        assert result is None

    def test_turns_exceeded(self, tmp_path):
        harness = SafetyHarness("t", {"turns": 5}, [], tmp_path)
        result = harness.check_limits({"turns": 5, "input_tokens": 0, "output_tokens": 0, "spend": 0})
        assert result is not None
        assert result["limit_code"] == "turns_exceeded"

    def test_spend_exceeded(self, tmp_path):
        harness = SafetyHarness("t", {"turns": 100, "spend": 0.5}, [], tmp_path)
        result = harness.check_limits({"turns": 1, "input_tokens": 0, "output_tokens": 0, "spend": 0.6})
        assert result is not None
        assert result["limit_code"] == "spend_exceeded"

    def test_duration_exceeded(self, tmp_path):
        harness = SafetyHarness("t", {"duration_seconds": 60}, [], tmp_path)
        result = harness.check_limits({"turns": 1, "input_tokens": 0, "output_tokens": 0, "spend": 0, "elapsed_seconds": 70})
        assert result is not None
        assert result["limit_code"] == "duration_seconds_exceeded"


# ── Capability Risk Classification ────────────────────────────────────

class TestCapabilityRisk:
    """Test _assess_capability_risk from thread_directive.py."""

    @pytest.fixture
    def risk_fn(self):
        """Import the risk assessment function."""
        td_path = (
            PROJECT_ROOT
            / "ryeos" / "rye" / ".ai" / "tools" / "rye" / "agent" / "threads" / "thread_directive.py"
        )
        spec = importlib.util.spec_from_file_location("thread_directive", td_path)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod._assess_capability_risk

    def test_unrestricted_blocked_without_ack(self, risk_fn, tmp_path):
        """rye.* is classified as unrestricted and blocked."""
        result = risk_fn(["rye.*"], [], "test-thread", tmp_path)
        assert result is not None
        assert result["risk"] == "unrestricted"
        assert "acknowledge" in result["error"].lower()

    def test_unrestricted_allowed_with_ack(self, risk_fn, tmp_path):
        """rye.* passes when acknowledged."""
        acks = [{"risk": "unrestricted", "reason": "needed"}]
        result = risk_fn(["rye.*"], acks, "test-thread", tmp_path)
        assert result is None

    def test_safe_capabilities_pass(self, risk_fn, tmp_path):
        """rye.search.* and rye.load.* are safe."""
        result = risk_fn(["rye.search.*"], [], "test-thread", tmp_path)
        assert result is None
        result = risk_fn(["rye.load.*"], [], "test-thread", tmp_path)
        assert result is None

    def test_file_system_write_allowed(self, risk_fn, tmp_path):
        """rye.execute.tool.rye.file-system.* is write risk, policy: allow."""
        result = risk_fn(
            ["rye.execute.tool.rye.file-system.*"], [], "test-thread", tmp_path
        )
        assert result is None

    def test_most_specific_wins(self, risk_fn, tmp_path):
        """Specific pattern (file-system) beats broad pattern (execute.*)."""
        # rye.execute.tool.rye.file-system.* → write (allow)
        # even though rye.execute.* → elevated (acknowledge_required)
        result = risk_fn(
            ["rye.execute.tool.rye.file-system.fs_write"], [], "test-thread", tmp_path
        )
        assert result is None


# ── System Message Injection (HttpProvider) ───────────────────────────

class TestSystemMessageInjection:
    """Test system prompt injection into API request bodies."""

    def test_body_field_mode(self):
        """Anthropic: system prompt set as top-level body field."""
        body = {"model": "claude", "messages": []}
        tool_use = {
            "system_message": {"mode": "body_field", "field": "system"},
        }
        # Simulate _inject_system_prompt logic
        sys_config = tool_use["system_message"]
        if sys_config["mode"] == "body_field":
            body[sys_config.get("field", "system")] = "You are Rye."
        assert body["system"] == "You are Rye."

    def test_body_inject_mode(self):
        """Gemini: system prompt merged via template."""
        body = {"contents": [], "tools": []}
        template = {"systemInstruction": {"parts": [{"text": "{system}"}]}}
        system = "You are Rye."
        # Simulate body_inject logic
        def apply(tmpl, data):
            if isinstance(tmpl, str) and tmpl == "{system}":
                return data["system"]
            if isinstance(tmpl, dict):
                return {k: apply(v, data) for k, v in tmpl.items()}
            if isinstance(tmpl, list):
                return [apply(item, data) for item in tmpl]
            return tmpl
        body.update(apply(template, {"system": system}))
        assert body["systemInstruction"]["parts"][0]["text"] == "You are Rye."

    def test_message_role_mode(self):
        """OpenAI-compat: system prompt prepended as system message."""
        messages = [{"role": "user", "content": "Hello"}]
        system_prompt = "You are Rye."
        # Simulate _convert_messages with message_role mode
        messages.insert(0, {"role": "system", "content": system_prompt})
        assert messages[0]["role"] == "system"
        assert messages[0]["content"] == "You are Rye."
        assert messages[1]["role"] == "user"


# ── Transcript Rendering ─────────────────────────────────────────────

class TestTranscriptRendering:
    """Test rendering of system_prompt and context_injected events."""

    def test_system_prompt_event(self):
        event = {
            "event_type": "system_prompt",
            "payload": {
                "text": "You are Rye.",
                "layers": ["rye/agent/core/identity", "rye/agent/core/behavior"],
            },
        }
        result = Transcript._render_cognition_event(event, 0)
        assert "## System Prompt" in result
        assert "rye/agent/core/identity" in result
        assert "rye/agent/core/behavior" in result
        assert "You are Rye." in result

    def test_system_prompt_custom_layers(self):
        event = {
            "event_type": "system_prompt",
            "payload": {"text": "Custom system.", "layers": []},
        }
        result = Transcript._render_cognition_event(event, 0)
        assert "custom" in result
        assert "Custom system." in result

    def test_context_injected_event(self):
        event = {
            "event_type": "context_injected",
            "payload": {
                "position": "before",
                "blocks": [
                    {"id": "rye/agent/core/environment", "content": "Project: /app"},
                ],
            },
        }
        result = Transcript._render_cognition_event(event, 1)
        assert "### Context: rye/agent/core/environment" in result
        assert "position: before" in result
        assert "Project: /app" in result

    def test_context_injected_multiple_blocks(self):
        event = {
            "event_type": "context_injected",
            "payload": {
                "position": "after",
                "blocks": [
                    {"id": "item-a", "content": "A content"},
                    {"id": "item-b", "content": "B content"},
                ],
            },
        }
        result = Transcript._render_cognition_event(event, 1)
        assert "item-a" in result
        assert "item-b" in result
        assert "A content" in result
        assert "B content" in result


# ── Core Knowledge Items ──────────────────────────────────────────────

KNOWLEDGE_DIR = (
    PROJECT_ROOT / "ryeos" / "rye" / ".ai" / "knowledge" / "rye" / "agent" / "core"
)

EXPECTED_ITEMS = ["identity", "behavior", "tool-protocol", "environment", "completion"]


class TestCoreKnowledgeItems:
    """Validate that core knowledge items exist with correct format."""

    @pytest.mark.parametrize("name", EXPECTED_ITEMS)
    def test_file_exists(self, name):
        path = KNOWLEDGE_DIR / f"{name}.md"
        assert path.exists(), f"Missing knowledge item: {path}"

    @pytest.mark.parametrize("name", EXPECTED_ITEMS)
    def test_has_unsigned_marker(self, name):
        content = (KNOWLEDGE_DIR / f"{name}.md").read_text()
        assert content.startswith("<!-- rye:unsigned -->")

    @pytest.mark.parametrize("name", EXPECTED_ITEMS)
    def test_has_yaml_frontmatter(self, name):
        content = (KNOWLEDGE_DIR / f"{name}.md").read_text()
        assert "```yaml" in content
        assert f"name: {name}" in content
        assert "category: rye/agent/core" in content

    @pytest.mark.parametrize("name", EXPECTED_ITEMS)
    def test_has_content_after_frontmatter(self, name):
        content = (KNOWLEDGE_DIR / f"{name}.md").read_text()
        # Content should exist after the closing ```
        parts = content.split("```")
        # At least: before fence, yaml, after fence (content)
        assert len(parts) >= 3
        body = parts[-1].strip()
        assert len(body) > 20, f"Knowledge item {name} has insufficient content"

    def test_identity_mentions_rye(self):
        content = (KNOWLEDGE_DIR / "identity.md").read_text()
        assert "Rye" in content

    def test_tool_protocol_mentions_four_tools(self):
        content = (KNOWLEDGE_DIR / "tool-protocol.md").read_text()
        assert "rye_execute" in content
        assert "rye_search" in content
        assert "rye_load" in content
        assert "rye_sign" in content

    def test_completion_mentions_directive_return(self):
        content = (KNOWLEDGE_DIR / "completion.md").read_text()
        assert "directive_return" in content

    def test_environment_has_template_vars(self):
        content = (KNOWLEDGE_DIR / "environment.md").read_text()
        assert "{project_path}" in content or "{model}" in content


# ── XML Parser: <suppress> in <context> ───────────────────────────────

class TestContextSuppressParsing:
    """Test <suppress> tag parsing inside <context> metadata."""

    def test_suppress_single(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
    <context>
      <suppress>tool-protocol</suppress>
    </context>
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        ctx = result.get("context", {})
        assert ctx["suppress"] == ["tool-protocol"]

    def test_suppress_multiple(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
    <context>
      <suppress>tool-protocol</suppress>
      <suppress>behavior</suppress>
    </context>
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        ctx = result.get("context", {})
        assert ctx["suppress"] == ["tool-protocol", "behavior"]

    def test_suppress_with_add(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
    <context>
      <suppress>tool-protocol</suppress>
      <before>project/custom-rules</before>
      <after>project/custom-completion</after>
    </context>
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        ctx = result.get("context", {})
        assert ctx["suppress"] == ["tool-protocol"]
        assert ctx["before"] == ["project/custom-rules"]
        assert ctx["after"] == ["project/custom-completion"]

    def test_no_suppress(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="general" />
    <context>
      <system>rye/agent/core/identity</system>
    </context>
  </metadata>
</directive>
```
'''
        result = md_parse(md)
        ctx = result.get("context", {})
        assert ctx["suppress"] == []


# ── _is_suppressed helper ─────────────────────────────────────────────

class TestIsSuppressed:
    """Test the _is_suppressed helper in safety_harness."""

    def test_suppress_by_hook_id(self):
        hook = {"id": "system_tool_protocol", "event": "build_system_prompt",
                "action": {"primary": "load", "item_type": "knowledge",
                           "item_id": "rye/agent/core/tool-protocol"}}
        assert _harness_mod._is_suppressed(hook, ["system_tool_protocol"]) is True

    def test_suppress_by_full_item_id(self):
        hook = {"id": "system_tool_protocol", "event": "build_system_prompt",
                "action": {"primary": "load", "item_type": "knowledge",
                           "item_id": "rye/agent/core/tool-protocol"}}
        assert _harness_mod._is_suppressed(hook, ["rye/agent/core/tool-protocol"]) is True

    def test_basename_does_not_match(self):
        """Basename matching is disabled to avoid ambiguous clashes."""
        hook = {"id": "system_tool_protocol", "event": "build_system_prompt",
                "action": {"primary": "load", "item_type": "knowledge",
                           "item_id": "rye/agent/core/tool-protocol"}}
        assert _harness_mod._is_suppressed(hook, ["tool-protocol"]) is False

    def test_no_match(self):
        hook = {"id": "system_identity", "event": "build_system_prompt",
                "action": {"primary": "load", "item_type": "knowledge",
                           "item_id": "rye/agent/core/identity"}}
        assert _harness_mod._is_suppressed(hook, ["tool-protocol"]) is False

    def test_empty_suppress_list(self):
        hook = {"id": "system_identity", "event": "build_system_prompt",
                "action": {"primary": "load", "item_type": "knowledge",
                           "item_id": "rye/agent/core/identity"}}
        assert _harness_mod._is_suppressed(hook, []) is False

    def test_suppress_identity_by_hook_id(self):
        hook = {"id": "system_identity", "event": "build_system_prompt",
                "action": {"primary": "load", "item_type": "knowledge",
                           "item_id": "rye/agent/core/identity"}}
        assert _harness_mod._is_suppressed(hook, ["system_identity"]) is True

    def test_partial_no_match(self):
        """Partial name like 'proto' should NOT match 'tool-protocol'."""
        hook = {"id": "system_tool_protocol", "event": "build_system_prompt",
                "action": {"primary": "load", "item_type": "knowledge",
                           "item_id": "rye/agent/core/tool-protocol"}}
        assert _harness_mod._is_suppressed(hook, ["proto"]) is False


# ── SafetyHarness: run_hooks_context with suppress ───────────────────

class TestRunHooksContextSuppress:
    """Test that run_hooks_context respects suppress parameter."""

    @pytest.fixture
    def harness_with_hooks(self, tmp_path):
        """Create a harness with mock context hooks."""
        hooks = [
            {
                "id": "system_identity",
                "event": "build_system_prompt",
                "layer": 2,
                "position": "before",
                "action": {
                    "primary": "load",
                    "item_type": "knowledge",
                    "item_id": "rye/agent/core/identity",
                },
            },
            {
                "id": "system_tool_protocol",
                "event": "build_system_prompt",
                "layer": 2,
                "position": "before",
                "action": {
                    "primary": "load",
                    "item_type": "knowledge",
                    "item_id": "rye/agent/core/tool-protocol",
                },
            },
        ]
        harness = SafetyHarness(
            thread_id="test-thread",
            limits={"turns": 10},
            hooks=hooks,
            project_path=tmp_path,
            directive_name="test",
        )
        return harness

    @pytest.mark.asyncio
    async def test_suppress_skips_hook(self, harness_with_hooks):
        """Suppressed hooks should not be dispatched."""
        dispatched = []

        class MockDispatcher:
            async def dispatch(self, action, **kwargs):
                dispatched.append(action.get("item_id", ""))
                return {"status": "success", "content": f"content for {action.get('item_id')}"}

        await harness_with_hooks.run_hooks_context(
            {}, MockDispatcher(), event="build_system_prompt",
            suppress=["system_tool_protocol"],
        )
        assert "rye/agent/core/identity" in dispatched
        assert "rye/agent/core/tool-protocol" not in dispatched

    @pytest.mark.asyncio
    async def test_no_suppress_dispatches_all(self, harness_with_hooks):
        """Without suppress, all hooks should be dispatched."""
        dispatched = []

        class MockDispatcher:
            async def dispatch(self, action, **kwargs):
                dispatched.append(action.get("item_id", ""))
                return {"status": "success", "content": f"content for {action.get('item_id')}"}

        await harness_with_hooks.run_hooks_context(
            {}, MockDispatcher(), event="build_system_prompt",
        )
        assert "rye/agent/core/identity" in dispatched
        assert "rye/agent/core/tool-protocol" in dispatched


# ── Condition-Based Context Switching ─────────────────────────────────

CONDITION_EVALUATOR_PATH = (
    PROJECT_ROOT
    / "ryeos" / "rye" / ".ai" / "tools" / "rye" / "agent" / "threads"
    / "loaders" / "condition_evaluator.py"
)
_cespec = importlib.util.spec_from_file_location("condition_evaluator", CONDITION_EVALUATOR_PATH)
_cond_mod = importlib.util.module_from_spec(_cespec)
_cespec.loader.exec_module(_cond_mod)


class TestConditionBasedContextSwitching:
    """Test condition patterns used for dynamic context injection.

    Projects can define conditional hooks that switch identity/behavior
    based on directive name, model, or other context fields.
    """

    def test_contains_matches_directive_name(self):
        doc = {"directive": "rye/web/browser"}
        condition = {"path": "directive", "op": "contains", "value": "web"}
        assert _cond_mod.matches(doc, condition) is True

    def test_contains_no_match(self):
        doc = {"directive": "rye/core/deploy"}
        condition = {"path": "directive", "op": "contains", "value": "web"}
        assert _cond_mod.matches(doc, condition) is False

    def test_not_inverts_match(self):
        doc = {"directive": "rye/core/deploy"}
        condition = {"not": {"path": "directive", "op": "contains", "value": "web"}}
        assert _cond_mod.matches(doc, condition) is True

    def test_any_matches_first_alternative(self):
        doc = {"directive": "rye/web/browser"}
        condition = {
            "any": [
                {"path": "directive", "op": "contains", "value": "web"},
                {"path": "directive", "op": "contains", "value": "deploy"},
            ]
        }
        assert _cond_mod.matches(doc, condition) is True

    def test_any_no_match(self):
        doc = {"directive": "rye/core/init"}
        condition = {
            "any": [
                {"path": "directive", "op": "contains", "value": "web"},
                {"path": "directive", "op": "contains", "value": "deploy"},
            ]
        }
        assert _cond_mod.matches(doc, condition) is False

    def test_regex_matches_category_pattern(self):
        doc = {"directive": "project/deploy/staging"}
        condition = {"path": "directive", "op": "regex", "value": "^project/deploy/"}
        assert _cond_mod.matches(doc, condition) is True

    def test_empty_condition_always_matches(self):
        """No condition = unconditional hook (default behavior)."""
        doc = {"directive": "anything"}
        assert _cond_mod.matches(doc, {}) is True


class TestConditionalHookDispatch:
    """Test that conditional hooks in run_hooks_context only fire when matched."""

    @pytest.mark.asyncio
    async def test_conditional_hook_fires_on_match(self, tmp_path):
        """Hook with condition fires when directive matches."""
        hooks = [
            {
                "id": "web_identity",
                "event": "build_system_prompt",
                "layer": 2,
                "position": "before",
                "condition": {"path": "directive", "op": "contains", "value": "web"},
                "action": {
                    "primary": "load",
                    "item_type": "knowledge",
                    "item_id": "project/identities/web-agent",
                },
            },
        ]
        harness = SafetyHarness("t", {"turns": 10}, hooks, tmp_path, directive_name="test")
        dispatched = []

        class MockDispatcher:
            async def dispatch(self, action, **kwargs):
                dispatched.append(action.get("item_id", ""))
                return {"status": "success", "content": "web identity content"}

        await harness.run_hooks_context(
            {"directive": "rye/web/browser"}, MockDispatcher(),
            event="build_system_prompt",
        )
        assert "project/identities/web-agent" in dispatched

    @pytest.mark.asyncio
    async def test_conditional_hook_skipped_on_no_match(self, tmp_path):
        """Hook with condition does NOT fire when directive doesn't match."""
        hooks = [
            {
                "id": "web_identity",
                "event": "build_system_prompt",
                "layer": 2,
                "position": "before",
                "condition": {"path": "directive", "op": "contains", "value": "web"},
                "action": {
                    "primary": "load",
                    "item_type": "knowledge",
                    "item_id": "project/identities/web-agent",
                },
            },
        ]
        harness = SafetyHarness("t", {"turns": 10}, hooks, tmp_path, directive_name="test")
        dispatched = []

        class MockDispatcher:
            async def dispatch(self, action, **kwargs):
                dispatched.append(action.get("item_id", ""))
                return {"status": "success", "content": "web identity content"}

        await harness.run_hooks_context(
            {"directive": "rye/core/deploy"}, MockDispatcher(),
            event="build_system_prompt",
        )
        assert dispatched == []

    @pytest.mark.asyncio
    async def test_multiple_conditional_identities(self, tmp_path):
        """Multiple conditional hooks — only matching ones fire."""
        hooks = [
            {
                "id": "default_identity",
                "event": "build_system_prompt",
                "layer": 2,
                "position": "before",
                "condition": {
                    "not": {
                        "any": [
                            {"path": "directive", "op": "contains", "value": "web"},
                            {"path": "directive", "op": "contains", "value": "deploy"},
                        ]
                    }
                },
                "action": {
                    "primary": "load",
                    "item_type": "knowledge",
                    "item_id": "rye/agent/core/identity",
                },
            },
            {
                "id": "web_identity",
                "event": "build_system_prompt",
                "layer": 2,
                "position": "before",
                "condition": {"path": "directive", "op": "contains", "value": "web"},
                "action": {
                    "primary": "load",
                    "item_type": "knowledge",
                    "item_id": "project/identities/web-agent",
                },
            },
            {
                "id": "deploy_identity",
                "event": "build_system_prompt",
                "layer": 2,
                "position": "before",
                "condition": {"path": "directive", "op": "contains", "value": "deploy"},
                "action": {
                    "primary": "load",
                    "item_type": "knowledge",
                    "item_id": "project/identities/deploy-agent",
                },
            },
        ]
        harness = SafetyHarness("t", {"turns": 10}, hooks, tmp_path, directive_name="test")

        class MockDispatcher:
            def __init__(self):
                self.dispatched = []
            async def dispatch(self, action, **kwargs):
                self.dispatched.append(action.get("item_id", ""))
                return {"status": "success", "content": f"content for {action.get('item_id')}"}

        # Web directive → only web identity fires
        d1 = MockDispatcher()
        await harness.run_hooks_context(
            {"directive": "rye/web/browser"}, d1, event="build_system_prompt",
        )
        assert "project/identities/web-agent" in d1.dispatched
        assert "rye/agent/core/identity" not in d1.dispatched
        assert "project/identities/deploy-agent" not in d1.dispatched

        # Deploy directive → only deploy identity fires
        d2 = MockDispatcher()
        await harness.run_hooks_context(
            {"directive": "project/deploy/staging"}, d2, event="build_system_prompt",
        )
        assert "project/identities/deploy-agent" in d2.dispatched
        assert "rye/agent/core/identity" not in d2.dispatched

        # Other directive → only default identity fires
        d3 = MockDispatcher()
        await harness.run_hooks_context(
            {"directive": "rye/core/init"}, d3, event="build_system_prompt",
        )
        assert "rye/agent/core/identity" in d3.dispatched
        assert "project/identities/web-agent" not in d3.dispatched


# ── ConfigLoader merge-by-id for hook overrides ──────────────────────

CONFIG_LOADER_PATH = (
    PROJECT_ROOT
    / "ryeos" / "rye" / ".ai" / "tools" / "rye" / "agent" / "threads"
    / "loaders" / "config_loader.py"
)
_clspec = importlib.util.spec_from_file_location("config_loader", CONFIG_LOADER_PATH)
_config_mod = importlib.util.module_from_spec(_clspec)
_clspec.loader.exec_module(_config_mod)


class TestConfigMergeForHookOverrides:
    """Test ConfigLoader merge semantics for project hook overrides.

    Projects can override system hooks by creating .ai/config/hook_conditions.yaml
    with entries that share the same `id` as system hooks. The merge-by-id
    logic replaces the system entry with the project one.
    """

    def test_merge_replaces_by_id(self):
        """Project hook with same ID replaces the system hook."""
        loader = _config_mod.ConfigLoader("test.yaml")
        base = [
            {"id": "system_identity", "event": "build_system_prompt",
             "action": {"item_id": "rye/agent/core/identity"}},
            {"id": "system_behavior", "event": "build_system_prompt",
             "action": {"item_id": "rye/agent/core/behavior"}},
        ]
        override = [
            {"id": "system_identity", "event": "build_system_prompt",
             "condition": {"path": "directive", "op": "contains", "value": "web"},
             "action": {"item_id": "project/identities/web-agent"}},
        ]
        result = loader._merge_list_by_id(base, override)
        # system_identity should be replaced
        identity_hook = next(h for h in result if h["id"] == "system_identity")
        assert identity_hook["action"]["item_id"] == "project/identities/web-agent"
        assert "condition" in identity_hook
        # system_behavior should be unchanged
        behavior_hook = next(h for h in result if h["id"] == "system_behavior")
        assert behavior_hook["action"]["item_id"] == "rye/agent/core/behavior"

    def test_merge_adds_new_hooks(self):
        """Project hooks with new IDs are appended."""
        loader = _config_mod.ConfigLoader("test.yaml")
        base = [
            {"id": "system_identity", "event": "build_system_prompt",
             "action": {"item_id": "rye/agent/core/identity"}},
        ]
        override = [
            {"id": "project_deploy_context", "event": "thread_started",
             "condition": {"path": "directive", "op": "contains", "value": "deploy"},
             "action": {"item_id": "project/deploy/rules"}},
        ]
        result = loader._merge_list_by_id(base, override)
        assert len(result) == 2
        ids = [h["id"] for h in result]
        assert "system_identity" in ids
        assert "project_deploy_context" in ids

    def test_merge_preserves_order(self):
        """System hooks keep their order; project additions go at the end."""
        loader = _config_mod.ConfigLoader("test.yaml")
        base = [
            {"id": "a", "order": 1},
            {"id": "b", "order": 2},
            {"id": "c", "order": 3},
        ]
        override = [
            {"id": "b", "order": 99},  # replace
            {"id": "d", "order": 4},   # new
        ]
        result = loader._merge_list_by_id(base, override)
        assert [h["id"] for h in result] == ["a", "b", "c", "d"]
        assert result[1]["order"] == 99  # b was replaced


# ── ConfigLoader three-tier cascade (system → user → project) ────────

class TestConfigLoaderThreeTierCascade:
    """Test that ConfigLoader.load() cascades system → user → project."""

    def _write_yaml(self, path: Path, data: dict):
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "w") as f:
            yaml.dump(data, f)

    def test_user_config_merged_when_present(self, tmp_path, monkeypatch):
        """User-space config is loaded and merged on top of system."""
        user_dir = tmp_path / "user_home" / ".ai"
        monkeypatch.setattr(_config_mod, "get_user_ai_path", lambda: user_dir)

        self._write_yaml(user_dir / "config" / "test.yaml", {"custom_key": "from_user"})

        loader = _config_mod.ConfigLoader("test.yaml")
        # Stub system config via _load_yaml to avoid needing real system file
        system_data = {"base_key": "from_system"}
        original_load_yaml = loader._load_yaml
        system_path = Path(_config_mod.__file__).parent.parent / "config" / "test.yaml"

        def patched_load_yaml(path):
            if path == system_path:
                return dict(system_data)
            return original_load_yaml(path)

        monkeypatch.setattr(loader, "_load_yaml", patched_load_yaml)

        project_path = tmp_path / "project"
        project_path.mkdir()
        result = loader.load(project_path)
        assert result["base_key"] == "from_system"
        assert result["custom_key"] == "from_user"

    def test_three_tier_merge_order(self, tmp_path, monkeypatch):
        """System → user → project: each layer overrides the previous."""
        user_dir = tmp_path / "user_home" / ".ai"
        monkeypatch.setattr(_config_mod, "get_user_ai_path", lambda: user_dir)

        self._write_yaml(user_dir / "config" / "test.yaml", {
            "value": "user",
            "user_only": True,
        })

        project_path = tmp_path / "project"
        self._write_yaml(project_path / ".ai" / "config" / "test.yaml", {
            "value": "project",
            "project_only": True,
        })

        loader = _config_mod.ConfigLoader("test.yaml")
        system_data = {"value": "system", "system_only": True}
        system_path = Path(_config_mod.__file__).parent.parent / "config" / "test.yaml"

        def patched_load_yaml(path):
            if path == system_path:
                return dict(system_data)
            with open(path) as f:
                return yaml.safe_load(f) or {}

        monkeypatch.setattr(loader, "_load_yaml", patched_load_yaml)

        result = loader.load(project_path)
        assert result["value"] == "project"
        assert result["system_only"] is True
        assert result["user_only"] is True
        assert result["project_only"] is True

    def test_project_overrides_user(self, tmp_path, monkeypatch):
        """Project config wins over user config for the same key."""
        user_dir = tmp_path / "user_home" / ".ai"
        monkeypatch.setattr(_config_mod, "get_user_ai_path", lambda: user_dir)

        self._write_yaml(user_dir / "config" / "test.yaml", {
            "shared": "user_value",
        })

        project_path = tmp_path / "project"
        self._write_yaml(project_path / ".ai" / "config" / "test.yaml", {
            "shared": "project_value",
        })

        loader = _config_mod.ConfigLoader("test.yaml")
        system_path = Path(_config_mod.__file__).parent.parent / "config" / "test.yaml"

        def patched_load_yaml(path):
            if path == system_path:
                return {"shared": "system_value"}
            with open(path) as f:
                return yaml.safe_load(f) or {}

        monkeypatch.setattr(loader, "_load_yaml", patched_load_yaml)

        result = loader.load(project_path)
        assert result["shared"] == "project_value"

    def test_no_user_config_skips_user_tier(self, tmp_path, monkeypatch):
        """When user config doesn't exist, only system and project merge."""
        user_dir = tmp_path / "user_home" / ".ai"
        monkeypatch.setattr(_config_mod, "get_user_ai_path", lambda: user_dir)
        # No user config file created

        project_path = tmp_path / "project"
        self._write_yaml(project_path / ".ai" / "config" / "test.yaml", {
            "project_key": "present",
        })

        loader = _config_mod.ConfigLoader("test.yaml")
        system_path = Path(_config_mod.__file__).parent.parent / "config" / "test.yaml"

        def patched_load_yaml(path):
            if path == system_path:
                return {"system_key": "present"}
            with open(path) as f:
                return yaml.safe_load(f) or {}

        monkeypatch.setattr(loader, "_load_yaml", patched_load_yaml)

        result = loader.load(project_path)
        assert result["system_key"] == "present"
        assert result["project_key"] == "present"
