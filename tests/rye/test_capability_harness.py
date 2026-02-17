"""Tests for capability harness system.

Tests cover:
- Capability token creation and validation
- Permission checking (allow/deny scenarios)
- Thread spawn attenuation
- Safety harness limit enforcement
- Integration with execute tool (signed directives)
"""

import importlib.util
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from rye.tools.sign import SignTool
from rye.tools.execute import ExecuteTool

# Import capability tokens module using importlib to avoid package conflicts
TOKENS_PATH = Path(__file__).parent.parent.parent / "rye" / "rye" / ".ai" / "tools" / "rye" / "agent" / "permissions" / "capability_tokens" / "capability_tokens.py"
spec = importlib.util.spec_from_file_location("capability_tokens", TOKENS_PATH)
capability_tokens_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capability_tokens_module)

CapabilityToken = capability_tokens_module.CapabilityToken
expand_capabilities = capability_tokens_module.expand_capabilities
check_capability = capability_tokens_module.check_capability
check_all_capabilities = capability_tokens_module.check_all_capabilities
item_id_to_cap = capability_tokens_module.item_id_to_cap
parse_capability = capability_tokens_module.parse_capability
cap_matches = capability_tokens_module.cap_matches
get_primary_tools_for_caps = capability_tokens_module.get_primary_tools_for_caps
attenuate_token = capability_tokens_module.attenuate_token
is_system_capability = capability_tokens_module.is_system_capability
load_capabilities = capability_tokens_module.load_capabilities

# Import safety harness
THREADS_PATH = Path(__file__).parent.parent.parent / "rye" / "rye" / ".ai" / "tools" / "rye" / "agent" / "threads" / "safety_harness.py"
spec = importlib.util.spec_from_file_location("safety_harness", THREADS_PATH)
safety_harness_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(safety_harness_module)
SafetyHarness = safety_harness_module.SafetyHarness
HarnessAction = safety_harness_module.HarnessAction

# Import markdown_xml parser
PARSER_PATH = Path(__file__).parent.parent.parent / "rye" / "rye" / ".ai" / "tools" / "rye" / "core" / "parsers" / "markdown_xml.py"
parser_spec = importlib.util.spec_from_file_location("markdown_xml", PARSER_PATH)
markdown_xml_module = importlib.util.module_from_spec(parser_spec)
parser_spec.loader.exec_module(markdown_xml_module)
md_xml_parse = markdown_xml_module.parse


@pytest.fixture
def temp_project():
    """Create temporary project with signed test directives."""
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        ai_dir = project_root / ".ai"
        
        # Create directives directory
        directives_dir = ai_dir / "directives"
        directives_dir.mkdir(parents=True)
        
        # Create LLM config for pricing
        llm_dir = ai_dir / "tools" / "llm"
        llm_dir.mkdir(parents=True)
        (llm_dir / "pricing.yaml").write_text('''models:
  claude-sonnet-4-20250514:
    input_per_million: 3.0
    output_per_million: 15.0
default:
  input_per_million: 5.0
  output_per_million: 15.0
''')
        
        # Directive with filesystem read permission
        (directives_dir / "test_fs_read.md").write_text('''# Test FS Read Directive

```xml
<directive name="test_fs_read" version="1.0.0">
  <metadata>
    <description>Test filesystem read permission</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>3</turns>
      <tokens>1000</tokens>
    </limits>
    <permissions>
      <execute><tool>rye.file-system.fs_read</tool></execute>
    </permissions>
  </metadata>
  <inputs>
    <input name="path" type="string" required="true">Path to read</input>
  </inputs>
  <process>
    <step name="read">Read file at {path}</step>
  </process>
</directive>
```
''')
        
        # Directive with filesystem write permission
        (directives_dir / "test_fs_write.md").write_text('''# Test FS Write Directive

```xml
<directive name="test_fs_write" version="1.0.0">
  <metadata>
    <description>Test filesystem write permission</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>5</turns>
      <tokens>2000</tokens>
    </limits>
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>
  <inputs>
    <input name="path" type="string" required="true">Path to write</input>
    <input name="content" type="string" required="true">Content</input>
  </inputs>
  <process>
    <step name="write">Write {content} to {path}</step>
  </process>
</directive>
```
''')
        
        # Directive with thread spawn permission
        (directives_dir / "test_spawn.md").write_text('''# Test Spawn Directive

```xml
<directive name="test_spawn" version="1.0.0">
  <metadata>
    <description>Test thread spawn permission</description>
    <category />
    <author>test</author>
    <model tier="orchestrator" />
    <limits>
      <turns>10</turns>
      <tokens>5000</tokens>
      <max_spawns>2</max_spawns>
    </limits>
    <permissions>
      <execute>
        <tool>rye.agent.threads.spawn_thread</tool>
        <tool>rye.agent.threads.read_thread</tool>
        <tool>rye.file-system.fs_read</tool>
      </execute>
    </permissions>
  </metadata>
  <inputs>
    <input name="child_directive" type="string" required="true">Child directive</input>
  </inputs>
  <process>
    <step name="spawn">Spawn {child_directive}</step>
  </process>
</directive>
```
''')
        
        # Directive with new hierarchical permission format
        (directives_dir / "test_new_perms.md").write_text('''# Test New Permission Format

```xml
<directive name="test_new_perms" version="1.0.0">
  <metadata>
    <description>Test new hierarchical permission format</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>3</turns>
      <tokens>1000</tokens>
    </limits>
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>*</search>
    </permissions>
  </metadata>
  <inputs>
    <input name="query" type="string" required="true">Search query</input>
  </inputs>
  <process>
    <step name="search">Search and execute</step>
  </process>
</directive>
```
''')
        
        # Directive with no permissions
        (directives_dir / "test_no_perms.md").write_text('''# Test No Permissions Directive

```xml
<directive name="test_no_perms" version="1.0.0">
  <metadata>
    <description>Test directive with no permissions</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>2</turns>
      <tokens>500</tokens>
    </limits>
  </metadata>
  <inputs>
    <input name="message" type="string" required="true">Message</input>
  </inputs>
  <process>
    <step name="echo">Return {message}</step>
  </process>
</directive>
```
''')
        
        yield project_root


@pytest.fixture
async def signed_project(temp_project):
    """Sign all directives in the temp project."""
    sign_tool = SignTool("")
    
    for directive in ["test_fs_read", "test_fs_write", "test_spawn", "test_no_perms"]:
        result = await sign_tool.handle(
            item_type="directive",
            item_id=directive,
            project_path=str(temp_project),
            location="project",
        )
        assert result["status"] == "signed", f"Failed to sign {directive}: {result}"
    
    return temp_project


class TestCapabilityHierarchy:
    """Test capability hierarchy expansion."""
    
    def test_execute_implies_search_and_load(self):
        """rye.execute.* should imply rye.search.* and rye.load.*."""
        caps = ["rye.execute.*"]
        expanded = expand_capabilities(caps)
        assert "rye.search.*" in expanded
        assert "rye.load.*" in expanded
    
    def test_search_does_not_imply_execute(self):
        """rye.search.* should NOT imply rye.execute.*."""
        caps = ["rye.search.*"]
        expanded = expand_capabilities(caps)
        assert "rye.execute.*" not in expanded
    
    def test_execute_tool_implies_search_tool(self):
        """rye.execute.tool.* should imply rye.search.tool.* and rye.load.tool.*."""
        caps = ["rye.execute.tool.*"]
        expanded = expand_capabilities(caps)
        assert "rye.search.tool.*" in expanded
        assert "rye.load.tool.*" in expanded
    
    def test_sign_implies_load(self):
        """rye.sign.* should imply rye.load.*."""
        caps = ["rye.sign.*"]
        expanded = expand_capabilities(caps)
        assert "rye.load.*" in expanded
        assert "rye.execute.*" not in expanded


class TestCapabilityParsing:
    """Test capability parsing and matching."""
    
    def test_parse_full_cap(self):
        parsed = parse_capability("rye.execute.tool.rye.file-system.fs_write")
        assert parsed["primary"] == "execute"
        assert parsed["item_type"] == "tool"
        assert parsed["specifics"] == "rye.file-system.fs_write"
        assert not parsed["is_wildcard"]
    
    def test_parse_wildcard(self):
        parsed = parse_capability("rye.execute.tool.*")
        assert parsed["primary"] == "execute"
        assert parsed["item_type"] == "tool"
        assert parsed["is_wildcard"]
    
    def test_parse_god_mode(self):
        parsed = parse_capability("rye.*")
        assert parsed["primary"] == "*"
        assert parsed["is_wildcard"]
    
    def test_parse_primary_only(self):
        parsed = parse_capability("rye.execute")
        assert parsed["primary"] == "execute"
        assert parsed["item_type"] == "*"
        assert parsed["is_wildcard"]
    
    def test_cap_matches_exact(self):
        assert cap_matches(
            "rye.execute.tool.rye.file-system.fs_write",
            "rye.execute.tool.rye.file-system.fs_write",
        )
    
    def test_cap_matches_wildcard(self):
        assert cap_matches(
            "rye.execute.tool.*",
            "rye.execute.tool.rye.file-system.fs_write",
        )
    
    def test_cap_matches_primary_wildcard(self):
        assert cap_matches(
            "rye.execute.*",
            "rye.execute.tool.rye.file-system.fs_write",
        )
    
    def test_cap_no_match_wrong_primary(self):
        assert not cap_matches(
            "rye.search.*",
            "rye.execute.tool.rye.file-system.fs_write",
        )
    
    def test_item_id_to_cap(self):
        cap = item_id_to_cap("execute", "tool", "rye/file-system/fs_write")
        assert cap == "rye.execute.tool.rye.file-system.fs_write"
    
    def test_get_primary_tools(self):
        caps = ["rye.execute.tool.rye.file-system.*"]
        primaries = get_primary_tools_for_caps(caps)
        assert "execute" in primaries
        assert "search" in primaries
        assert "load" in primaries


class TestCapabilityChecking:
    """Test capability checking logic."""
    
    def test_direct_match(self):
        granted = [
            "rye.execute.tool.rye.file-system.fs_read",
            "rye.execute.tool.rye.file-system.fs_write",
        ]
        assert check_capability(granted, "rye.execute.tool.rye.file-system.fs_read")
    
    def test_implied_match(self):
        """rye.execute.* implies access to rye.search.tool.something."""
        granted = ["rye.execute.*"]
        assert check_capability(granted, "rye.search.tool.rye.file-system.fs_read")
    
    def test_missing_capability(self):
        """rye.search.* should NOT match rye.execute.tool.X."""
        granted = ["rye.search.*"]
        assert not check_capability(granted, "rye.execute.tool.rye.file-system.fs_write")
    
    def test_check_all_satisfied(self):
        granted = ["rye.execute.*"]
        satisfied, missing = check_all_capabilities(
            granted,
            ["rye.search.tool.rye.file-system.fs_read", "rye.load.directive.test"],
        )
        assert satisfied
        assert len(missing) == 0
    
    def test_check_all_with_missing(self):
        granted = ["rye.search.*"]
        satisfied, missing = check_all_capabilities(
            granted,
            ["rye.search.tool.rye.file-system.fs_read", "rye.execute.tool.rye.file-system.fs_write"],
        )
        assert not satisfied
        assert "rye.execute.tool.rye.file-system.fs_write" in missing


class TestTokenCreation:
    """Test capability token creation and validation."""
    
    def test_create_token(self):
        token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.fs_read", "rye.execute.tool.rye.file-system.fs_write"],
            aud="test-project",
            exp=datetime.now(timezone.utc) + timedelta(hours=24),
            directive_id="test_directive",
            thread_id="test-thread-1234",
        )
        assert token.caps == ["rye.execute.tool.rye.file-system.fs_read", "rye.execute.tool.rye.file-system.fs_write"]
        assert token.aud == "test-project"
    
    def test_token_not_expired(self):
        token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.fs_read"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=24),
            directive_id="test",
            thread_id="test",
        )
        assert not token.is_expired()
    
    def test_token_expired(self):
        token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.fs_read"],
            aud="test",
            exp=datetime.now(timezone.utc) - timedelta(hours=1),
            directive_id="test",
            thread_id="test",
        )
        assert token.is_expired()
    
    def test_token_has_capability_with_hierarchy(self):
        token = CapabilityToken(
            caps=["rye.execute.*"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=1),
            directive_id="test",
            thread_id="test",
        )
        assert token.has_capability("rye.search.tool.rye.file-system.fs_read")
    
    def test_token_serialization_roundtrip(self):
        token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.fs_read", "rye.execute.*"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=1),
            directive_id="test_dir",
            thread_id="thread-123",
        )
        
        token_str = token.to_jwt()
        restored = CapabilityToken.from_jwt(token_str)
        
        assert restored.caps == token.caps
        assert restored.thread_id == token.thread_id
        assert restored.directive_id == token.directive_id


class TestTokenAttenuation:
    """Test thread spawn capability attenuation."""
    
    def test_attenuation_intersection(self):
        parent_token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.*", "rye.execute.*", "rye.execute.tool.rye.agent.threads.spawn_thread"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=24),
            directive_id="parent",
            thread_id="parent-thread",
        )
        
        child_caps = ["rye.execute.tool.rye.file-system.*"]
        attenuated = attenuate_token(parent_token, child_caps)
        
        assert "rye.execute.tool.rye.file-system.*" in attenuated.caps
        assert "rye.execute.*" not in attenuated.caps
    
    def test_attenuation_no_escalation(self):
        parent_token = CapabilityToken(
            caps=["rye.search.*"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=24),
            directive_id="parent",
            thread_id="parent-thread",
        )
        
        child_caps = ["rye.execute.*", "rye.execute.tool.rye.db.write"]
        attenuated = attenuate_token(parent_token, child_caps)
        
        assert "rye.execute.*" not in attenuated.caps
        assert "rye.execute.tool.rye.db.write" not in attenuated.caps
    
    def test_attenuation_sets_parent_id(self):
        parent_token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.*"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=24),
            directive_id="parent",
            thread_id="parent-thread",
        )
        
        child_caps = ["rye.execute.tool.rye.file-system.*"]
        attenuated = attenuate_token(parent_token, child_caps)
        
        assert attenuated.parent_id == parent_token.token_id
    
    def test_attenuation_empty_child_caps(self):
        parent_token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.*", "rye.execute.*"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=24),
            directive_id="parent",
            thread_id="parent-thread",
        )
        
        child_caps = []
        attenuated = attenuate_token(parent_token, child_caps)
        
        assert len(attenuated.caps) == 0


class TestSystemCapabilityProtection:
    """Test that system capabilities cannot be overridden."""
    
    def test_rye_execute_is_system(self):
        assert is_system_capability("rye.execute")
    
    def test_rye_execute_tool_is_system(self):
        assert is_system_capability("rye.execute.tool.rye.file-system.fs_write")
    
    def test_rye_agent_thread_spawn_is_system(self):
        assert is_system_capability("rye.execute.tool.rye.agent.threads.spawn_thread")
    
    def test_custom_cap_not_system(self):
        assert not is_system_capability("custom.my_cap")
    
    def test_rye_wildcard_is_system(self):
        assert is_system_capability("rye.*")


class TestSafetyHarnessBasic:
    """Test basic safety harness functionality."""
    
    def test_initial_status(self, temp_project):
        harness = SafetyHarness(
            project_path=temp_project,
            limits={"turns": 5, "tokens": 1000},
            directive_name="test_directive",
            directive_inputs={"message": "hello"},
        )
        
        status = harness.get_status()
        assert status["directive"] == "test_directive"
        assert status["cost"]["turns"] == 0
        assert status["cost"]["tokens"] == 0
    
    def test_cost_tracking(self, temp_project):
        harness = SafetyHarness(
            project_path=temp_project,
            limits={"turns": 10},
            directive_name="test",
        )
        
        mock_response = {
            "usage": {"input_tokens": 100, "output_tokens": 50}
        }
        harness.update_cost_after_turn(mock_response, "claude-sonnet-4-20250514")
        
        assert harness.cost.turns == 1
        assert harness.cost.tokens == 150
    
    def test_state_serialization(self, temp_project):
        harness = SafetyHarness(
            project_path=temp_project,
            limits={"turns": 5},
            directive_name="test",
        )
        harness.cost.turns = 2
        harness.cost.tokens = 300
        
        state = harness.to_state_dict()
        restored = SafetyHarness.from_state_dict(state, temp_project)
        
        assert restored.cost.turns == 2
        assert restored.cost.tokens == 300
        assert restored.limits.get("turns") == 5


class TestSafetyHarnessLimits:
    """Test safety harness limit enforcement."""
    
    def test_under_limits_no_event(self, temp_project):
        harness = SafetyHarness(
            project_path=temp_project,
            limits={"turns": 10, "tokens": 1000},
            directive_name="test",
        )
        harness.cost.turns = 5
        harness.cost.tokens = 500
        
        event = harness.check_limits()
        assert event is None
    
    def test_at_turn_limit(self, temp_project):
        harness = SafetyHarness(
            project_path=temp_project,
            limits={"turns": 5},
            directive_name="test",
        )
        harness.cost.turns = 5
        
        event = harness.check_limits()
        assert event is not None
        assert event.get("name") == "limit"
        assert event.get("code") == "turns_exceeded"
    
    def test_over_token_limit(self, temp_project):
        harness = SafetyHarness(
            project_path=temp_project,
            limits={"tokens": 100},
            directive_name="test",
        )
        harness.cost.tokens = 150
        
        event = harness.check_limits()
        assert event is not None
        assert event.get("name") == "limit"
        assert event.get("code") == "tokens_exceeded"


class TestSafetyHarnessPermissions:
    """Test safety harness permission enforcement."""
    
    def test_has_required_permission(self, temp_project):
        parent_token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.fs_read", "rye.execute.tool.rye.file-system.*"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=1),
            directive_id="parent",
            thread_id="parent-thread",
        )
        
        harness = SafetyHarness(
            project_path=temp_project,
            directive_name="test_fs_read",
            parent_token=parent_token,
            required_permissions=[{"tag": "cap", "content": "rye.execute.tool.rye.file-system.fs_read"}],
        )
        
        event = harness.check_permissions()
        assert event is None
    
    def test_missing_required_permission(self, temp_project):
        parent_token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.fs_read"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=1),
            directive_id="parent",
            thread_id="parent-thread",
        )
        
        harness = SafetyHarness(
            project_path=temp_project,
            directive_name="test_fs_write",
            parent_token=parent_token,
            required_permissions=[{"tag": "cap", "content": "rye.execute.tool.rye.file-system.*"}],
        )
        
        event = harness.check_permissions()
        assert event is not None
        assert event.get("name") == "error"
        assert event.get("code") == "permission_denied"
    
    def test_no_token_no_requirements(self, temp_project):
        harness = SafetyHarness(
            project_path=temp_project,
            directive_name="test_no_perms",
        )
        
        event = harness.check_permissions()
        assert event is None

    def test_new_format_permissions(self, temp_project):
        parent_token = CapabilityToken(
            caps=["rye.execute.tool.rye.file-system.*", "rye.search.*"],
            aud="test",
            exp=datetime.now(timezone.utc) + timedelta(hours=1),
            directive_id="parent",
            thread_id="parent-thread",
        )

        harness = SafetyHarness(
            project_path=temp_project,
            directive_name="test_new_perms",
            parent_token=parent_token,
            required_permissions=[
                {"tag": "cap", "content": "rye.execute.tool.rye.file-system.*"},
                {"tag": "cap", "content": "rye.search.*"},
            ],
        )

        event = harness.check_permissions()
        assert event is None


class TestCapabilityYAMLLoading:
    """Test loading capabilities from YAML files."""
    
    def test_capabilities_load_without_error(self, temp_project):
        """System capabilities load without raising."""
        permissions, hierarchy = load_capabilities(temp_project)
        assert isinstance(permissions, dict)
        assert isinstance(hierarchy, dict)
    
    def test_hierarchy_loaded(self, temp_project):
        """Capability hierarchy is loaded (if any hierarchy entries exist)."""
        permissions, hierarchy = load_capabilities(temp_project)
        # hierarchy may be empty if primitives YAMLs no longer define hierarchy
        # (structural implication is now in PRIMARY_IMPLIES)
        assert isinstance(hierarchy, dict)


class TestNewPermissionFormat:
    """Test new hierarchical permission format parsing."""

    def test_god_mode_permissions(self):
        content = '''# Test

```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <category>test</category>
    <author>test</author>
    <model tier="general" />
    <permissions>*</permissions>
  </metadata>
</directive>
```
'''
        result = md_xml_parse(content)
        perms = result.get("permissions", [])
        assert len(perms) == 1
        assert perms[0]["tag"] == "cap"
        assert perms[0]["content"] == "rye.*"

    def test_primary_wildcard(self):
        content = '''# Test

```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <category>test</category>
    <author>test</author>
    <model tier="general" />
    <permissions><execute>*</execute></permissions>
  </metadata>
</directive>
```
'''
        result = md_xml_parse(content)
        perms = result.get("permissions", [])
        assert len(perms) == 1
        assert perms[0]["tag"] == "cap"
        assert perms[0]["content"] == "rye.execute.*"

    def test_hierarchical_tool_permissions(self):
        content = '''# Test

```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <category>test</category>
    <author>test</author>
    <model tier="general" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
    </permissions>
  </metadata>
</directive>
```
'''
        result = md_xml_parse(content)
        perms = result.get("permissions", [])
        assert len(perms) == 1
        assert perms[0]["tag"] == "cap"
        assert perms[0]["content"] == "rye.execute.tool.rye.file-system.*"

    def test_multiple_items(self):
        content = '''# Test

```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <category>test</category>
    <author>test</author>
    <model tier="general" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
        <tool>rye.agent.threads.spawn_thread</tool>
      </execute>
      <search>*</search>
    </permissions>
  </metadata>
</directive>
```
'''
        result = md_xml_parse(content)
        perms = result.get("permissions", [])
        caps = [p["content"] for p in perms]
        assert "rye.execute.tool.rye.file-system.*" in caps
        assert "rye.execute.tool.rye.agent.threads.spawn_thread" in caps
        assert "rye.search.*" in caps

    def test_mixed_item_types(self):
        content = '''# Test

```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <category>test</category>
    <author>test</author>
    <model tier="general" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <directive>analysis/*</directive>
      </search>
      <load>
        <knowledge>*</knowledge>
      </load>
    </permissions>
  </metadata>
</directive>
```
'''
        result = md_xml_parse(content)
        perms = result.get("permissions", [])
        caps = [p["content"] for p in perms]
        assert "rye.execute.tool.rye.file-system.*" in caps
        assert "rye.search.directive.analysis/*" in caps
        assert "rye.load.knowledge.*" in caps


@pytest.mark.asyncio
class TestExecuteWithSignedDirectives:
    """Test execute tool with signed directives."""
    
    async def test_execute_signed_directive(self, signed_project):
        execute_tool = ExecuteTool("")
        result = await execute_tool.handle(
            item_type="directive",
            item_id="test_fs_read",
            project_path=str(signed_project),
        )
        
        assert result["status"] == "success"
        assert result["type"] == "directive"
    
    async def test_execute_unsigned_directive_fails(self, temp_project):
        execute_tool = ExecuteTool("")
        
        directives_dir = temp_project / ".ai" / "directives"
        (directives_dir / "unsigned.md").write_text('''# Unsigned Directive

```xml
<directive name="unsigned" version="1.0.0">
  <metadata>
    <description>Unsigned test</description>
    <category>test</category>
    <author>test</author>
    <model tier="general">Test</model>
  </metadata>
  <process>
    <step name="test">Do something</step>
  </process>
</directive>
```
''')
        
        result = await execute_tool.handle(
            item_type="directive",
            item_id="unsigned",
            project_path=str(temp_project),
        )
        
        assert result["status"] == "error"
    
    async def test_dry_run_signed_directive(self, signed_project):
        execute_tool = ExecuteTool("")
        result = await execute_tool.handle(
            item_type="directive",
            item_id="test_fs_read",
            project_path=str(signed_project),
            dry_run=True,
        )
        
        assert result["status"] == "validation_passed"


@pytest.mark.asyncio
class TestSignDirectives:
    """Test signing directives."""
    
    async def test_sign_directive(self, temp_project):
        sign_tool = SignTool("")
        result = await sign_tool.handle(
            item_type="directive",
            item_id="test_fs_read",
            project_path=str(temp_project),
            location="project",
        )
        
        assert result["status"] == "signed"
        assert "signature" in result
        
        directive_file = temp_project / ".ai" / "directives" / "test_fs_read.md"
        content = directive_file.read_text()
        assert "rye:signed:" in content
    
    async def test_resign_replaces_signature(self, temp_project):
        sign_tool = SignTool("")
        
        await sign_tool.handle(
            item_type="directive",
            item_id="test_fs_read",
            project_path=str(temp_project),
            location="project",
        )
        
        await sign_tool.handle(
            item_type="directive",
            item_id="test_fs_read",
            project_path=str(temp_project),
            location="project",
        )
        
        directive_file = temp_project / ".ai" / "directives" / "test_fs_read.md"
        content = directive_file.read_text()
        assert content.count("rye:signed:") == 1
