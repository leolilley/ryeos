"""Tests for system prompt construction, user prompt building, and action parsing.

Covers:
- _build_system_prompt renders from provider template (data-driven)
- _build_system_prompt errors when no provider config or no template
- _format_steps_block and _format_inputs_block helpers
- _render_action for all 4 primary tools (execute, search, load, sign)
- <execute>, <search>, <load>, <sign> XML tag parsing
- _build_user_prompt extracts directive body correctly
- _strip_rye_signature removes validation signatures
- {input:name} placeholder resolution
"""

import importlib.util
import json
from pathlib import Path

import pytest
import yaml

PROJECT_ROOT = Path(__file__).parent.parent.parent

THREAD_DIRECTIVE_PATH = (
    PROJECT_ROOT
    / "rye" / "rye" / ".ai" / "tools" / "rye" / "agent" / "threads" / "thread_directive.py"
)
_spec = importlib.util.spec_from_file_location("thread_directive", THREAD_DIRECTIVE_PATH)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)

_build_system_prompt = _mod._build_system_prompt
_build_user_prompt = _mod._build_user_prompt
_format_steps_block = _mod._format_steps_block
_format_inputs_block = _mod._format_inputs_block
_strip_rye_signature = _mod._strip_rye_signature
_render_action = _mod._render_action
_resolve_input_refs = _mod._resolve_input_refs

PARSER_PATH = (
    PROJECT_ROOT
    / "rye" / "rye" / ".ai" / "tools" / "rye" / "core" / "parsers" / "markdown" / "xml.py"
)
_pspec = importlib.util.spec_from_file_location("markdown_xml", PARSER_PATH)
_parser_mod = importlib.util.module_from_spec(_pspec)
_pspec.loader.exec_module(_parser_mod)
md_parse = _parser_mod.parse

PROVIDER_YAML_PATH = (
    PROJECT_ROOT
    / "rye" / "rye" / ".ai" / "tools" / "rye" / "agent" / "providers" / "anthropic_messages.yaml"
)


@pytest.fixture
def provider_config():
    return yaml.safe_load(PROVIDER_YAML_PATH.read_text())


@pytest.fixture
def sample_directive():
    return {
        "description": "Write a hello world file",
        "steps": [
            {"name": "write_file", "description": "Write hello world to disk"},
            {"name": "verify", "description": "Confirm file exists"},
        ],
        "body": "# Hello World\n\nWrite hello world to a file.",
    }


@pytest.fixture
def sample_tool_defs():
    return [
        {"name": "rye_execute", "description": "Execute an item", "schema": {}},
        {"name": "rye_search", "description": "Search for items", "schema": {}},
    ]


class TestBuildSystemPrompt:
    """System prompt is rendered from provider template."""

    def test_renders_from_provider_template(self, provider_config, sample_directive, sample_tool_defs):
        result = _build_system_prompt(
            sample_directive,
            {"output_path": "/tmp/hello.txt"},
            sample_tool_defs,
            provider_config,
            "hello_world",
        )
        assert "Rye" in result
        assert "tool_use" in result
        assert "rye_execute, rye_search" in result
        assert "hello_world" in result
        assert "Write a hello world file" in result

    def test_includes_steps(self, provider_config, sample_directive, sample_tool_defs):
        result = _build_system_prompt(
            sample_directive, None, sample_tool_defs, provider_config, "test"
        )
        assert "write_file" in result
        assert "verify" in result

    def test_includes_inputs(self, provider_config, sample_directive, sample_tool_defs):
        result = _build_system_prompt(
            sample_directive,
            {"path": "/tmp/out.txt"},
            sample_tool_defs,
            provider_config,
            "test",
        )
        assert "/tmp/out.txt" in result

    def test_no_tools_shows_none(self, provider_config, sample_directive):
        result = _build_system_prompt(
            sample_directive, None, None, provider_config, "test"
        )
        assert "(none)" in result

    def test_empty_tool_defs_shows_none(self, provider_config, sample_directive):
        result = _build_system_prompt(
            sample_directive, None, [], provider_config, "test"
        )
        assert "(none)" in result

    def test_no_steps_omits_steps_block(self, provider_config, sample_tool_defs):
        directive = {"description": "Simple task"}
        result = _build_system_prompt(
            directive, None, sample_tool_defs, provider_config, "test"
        )
        assert "Steps:" not in result

    def test_no_inputs_omits_inputs_block(self, provider_config, sample_directive, sample_tool_defs):
        result = _build_system_prompt(
            sample_directive, None, sample_tool_defs, provider_config, "test"
        )
        assert "Inputs:" not in result

    def test_contains_tool_use_enforcement(self, provider_config, sample_directive, sample_tool_defs):
        result = _build_system_prompt(
            sample_directive, None, sample_tool_defs, provider_config, "test"
        )
        assert "NEVER" in result
        assert "XML" in result


class TestBuildSystemPromptErrors:
    """System prompt errors correctly when provider is missing or misconfigured."""

    def test_errors_without_provider_config(self, sample_directive):
        with pytest.raises(ValueError, match="no provider config loaded"):
            _build_system_prompt(sample_directive, None, [], None, "test")

    def test_errors_with_empty_provider_config(self, sample_directive):
        with pytest.raises(ValueError, match="missing 'prompts.system_template'"):
            _build_system_prompt(sample_directive, None, [], {}, "test")

    def test_errors_with_prompts_but_no_template(self, sample_directive):
        config = {"prompts": {}}
        with pytest.raises(ValueError, match="missing 'prompts.system_template'"):
            _build_system_prompt(sample_directive, None, [], config, "test")

    def test_errors_with_prompts_none(self, sample_directive):
        config = {"prompts": None}
        with pytest.raises(ValueError, match="missing 'prompts.system_template'"):
            _build_system_prompt(sample_directive, None, [], config, "test")

    def test_error_includes_directive_name(self, sample_directive):
        with pytest.raises(ValueError, match="my_directive"):
            _build_system_prompt(sample_directive, None, [], {}, "my_directive")


class TestCustomTemplate:
    """System prompt works with any provider template, not just the default."""

    def test_minimal_template(self, sample_directive):
        config = {
            "prompts": {
                "system_template": "Agent: {directive_name} | Tools: {tool_names}"
            }
        }
        tools = [{"name": "rye_execute", "description": "", "schema": {}}]
        result = _build_system_prompt(sample_directive, None, tools, config, "hello")
        assert result == "Agent: hello | Tools: rye_execute"

    def test_template_with_all_placeholders(self):
        config = {
            "prompts": {
                "system_template": (
                    "name={directive_name} "
                    "desc={directive_description} "
                    "tools={tool_names} "
                    "{directive_steps} "
                    "{directive_inputs}"
                )
            }
        }
        directive = {
            "description": "Do X",
            "steps": [{"name": "s1", "description": "step one"}],
        }
        tools = [{"name": "t1", "description": "", "schema": {}}]
        result = _build_system_prompt(directive, {"k": "v"}, tools, config, "d1")
        assert "name=d1" in result
        assert "desc=Do X" in result
        assert "tools=t1" in result
        assert "s1: step one" in result
        assert '"k": "v"' in result


class TestFormatHelpers:
    """_format_steps_block and _format_inputs_block."""

    def test_steps_block_empty(self):
        assert _format_steps_block({}) == ""
        assert _format_steps_block({"steps": []}) == ""

    def test_steps_block_formats(self):
        directive = {"steps": [
            {"name": "a", "description": "do A"},
            {"name": "b", "description": "do B"},
        ]}
        result = _format_steps_block(directive)
        assert result == "Steps:\n- a: do A\n- b: do B"

    def test_inputs_block_empty(self):
        assert _format_inputs_block(None) == ""
        assert _format_inputs_block({}) == ""

    def test_inputs_block_formats(self):
        result = _format_inputs_block({"path": "/tmp"})
        parsed = json.loads(result.replace("Inputs: ", ""))
        assert parsed == {"path": "/tmp"}


class TestBuildUserPrompt:
    """User prompt extracts directive body correctly."""

    def test_returns_body_when_present(self):
        directive = {"description": "desc", "body": "Do the thing."}
        assert _build_user_prompt(directive, None) == "Do the thing."

    def test_strips_rye_signature_from_body(self):
        directive = {
            "body": "<!-- rye:validated:2026-02-10T00:00:00Z:abc123 -->\n# Task\nDo it."
        }
        result = _build_user_prompt(directive, None)
        assert "rye:validated" not in result
        assert "# Task" in result
        assert "Do it." in result

    def test_falls_back_to_description(self):
        directive = {"description": "Write a file", "body": ""}
        result = _build_user_prompt(directive, None)
        assert "Write a file" in result

    def test_falls_back_to_generic(self):
        directive = {}
        result = _build_user_prompt(directive, None)
        assert "Execute the directive" in result

    def test_body_with_only_signature_falls_to_description(self):
        directive = {
            "description": "My desc",
            "body": "<!-- rye:validated:2026-02-10T00:00:00Z:abc -->",
        }
        result = _build_user_prompt(directive, None)
        assert "My desc" in result


class TestStripRyeSignature:
    """_strip_rye_signature removes validation comments."""

    def test_strips_signature(self):
        text = "<!-- rye:validated:2026-02-10T00:00:00Z:abc123def -->\nContent"
        assert _strip_rye_signature(text) == "Content"

    def test_preserves_non_signature_comments(self):
        text = "<!-- normal comment -->\nContent"
        assert "normal comment" in _strip_rye_signature(text)

    def test_strips_multiple_signatures(self):
        text = (
            "<!-- rye:validated:2026-01-01T00:00:00Z:aaa -->\n"
            "Middle\n"
            "<!-- rye:validated:2026-02-01T00:00:00Z:bbb -->\n"
            "End"
        )
        result = _strip_rye_signature(text)
        assert "rye:validated" not in result
        assert "Middle" in result
        assert "End" in result

    def test_no_signature_returns_unchanged(self):
        text = "Just some text"
        assert _strip_rye_signature(text) == "Just some text"


class TestProviderYAMLHasTemplate:
    """The actual anthropic_messages.yaml provider has a system_template."""

    def test_provider_yaml_has_prompts_section(self, provider_config):
        assert "prompts" in provider_config
        assert "system_template" in provider_config["prompts"]

    def test_template_has_required_placeholders(self, provider_config):
        template = provider_config["prompts"]["system_template"]
        assert "{tool_names}" in template
        assert "{directive_name}" in template
        assert "{directive_description}" in template
        assert "{directive_steps}" in template
        assert "{directive_inputs}" in template

    def test_template_mentions_tool_use(self, provider_config):
        template = provider_config["prompts"]["system_template"]
        assert "tool_use" in template

    def test_template_prohibits_xml(self, provider_config):
        template = provider_config["prompts"]["system_template"]
        assert "XML" in template


class TestResolveInputRefs:
    """{input:name} placeholder resolution."""

    def test_resolves_single_ref(self):
        assert _resolve_input_refs("{input:path}", {"path": "/tmp"}) == "/tmp"

    def test_resolves_multiple_refs(self):
        result = _resolve_input_refs(
            "{input:a} and {input:b}", {"a": "X", "b": "Y"}
        )
        assert result == "X and Y"

    def test_leaves_unmatched_refs(self):
        assert _resolve_input_refs("{input:missing}", {"other": "val"}) == "{input:missing}"

    def test_no_inputs_returns_unchanged(self):
        assert _resolve_input_refs("{input:x}", None) == "{input:x}"

    def test_no_refs_returns_unchanged(self):
        assert _resolve_input_refs("plain text", {"x": "y"}) == "plain text"


class TestRenderActionExecute:
    """_render_action for <execute> (rye_execute) with all 3 item types."""

    def test_execute_tool(self):
        action = {
            "primary": "execute",
            "item_type": "tool",
            "item_id": "rye/file-system/fs_write",
            "params": {"path": "/tmp/test.txt", "content": "hello"},
        }
        result = _render_action(action, None)
        assert "rye_execute" in result
        parsed = json.loads(result.split("\n")[1].strip())
        assert parsed["item_type"] == "tool"
        assert parsed["item_id"] == "rye/file-system/fs_write"
        assert parsed["parameters"]["path"] == "/tmp/test.txt"

    def test_execute_directive(self):
        action = {
            "primary": "execute",
            "item_type": "directive",
            "item_id": "hello_world",
            "params": {"output_path": "/tmp/hello.txt"},
        }
        result = _render_action(action, None)
        assert "rye_execute" in result
        parsed = json.loads(result.split("\n")[1].strip())
        assert parsed["item_type"] == "directive"
        assert parsed["item_id"] == "hello_world"
        assert parsed["parameters"]["inputs"]["output_path"] == "/tmp/hello.txt"

    def test_execute_knowledge(self):
        action = {
            "primary": "execute",
            "item_type": "knowledge",
            "item_id": "patterns/best_practices",
        }
        result = _render_action(action, None)
        parsed = json.loads(result.split("\n")[1].strip())
        assert parsed["item_type"] == "knowledge"
        assert parsed["item_id"] == "patterns/best_practices"

    def test_execute_resolves_input_refs(self):
        action = {
            "primary": "execute",
            "item_type": "tool",
            "item_id": "rye/file-system/fs_write",
            "params": {"path": "{input:output_path}"},
        }
        result = _render_action(action, {"output_path": "/resolved/path"})
        parsed = json.loads(result.split("\n")[1].strip())
        assert parsed["parameters"]["path"] == "/resolved/path"

    def test_execute_resolves_item_id_ref(self):
        action = {
            "primary": "execute",
            "item_type": "directive",
            "item_id": "{input:child_name}",
        }
        result = _render_action(action, {"child_name": "genesis_child"})
        parsed = json.loads(result.split("\n")[1].strip())
        assert parsed["item_id"] == "genesis_child"


class TestRenderActionSearchLoadSign:
    """_render_action for search, load, sign primary tools."""

    def test_search(self):
        action = {
            "primary": "search",
            "query": "directive patterns",
            "item_type": "knowledge",
        }
        result = _render_action(action, None)
        assert "rye_search" in result
        parsed = json.loads(result.split("\n")[1].strip())
        assert parsed["query"] == "directive patterns"
        assert parsed["item_type"] == "knowledge"

    def test_load(self):
        action = {
            "primary": "load",
            "item_type": "directive",
            "item_id": "hello_world",
        }
        result = _render_action(action, None)
        assert "rye_load" in result
        parsed = json.loads(result.split("\n")[1].strip())
        assert parsed["item_type"] == "directive"
        assert parsed["item_id"] == "hello_world"

    def test_sign(self):
        action = {
            "primary": "sign",
            "item_type": "directive",
            "item_id": "new_directive",
        }
        result = _render_action(action, None)
        assert "rye_sign" in result
        parsed = json.loads(result.split("\n")[1].strip())
        assert parsed["item_id"] == "new_directive"

    def test_search_with_input_refs(self):
        action = {
            "primary": "search",
            "query": "{input:topic}",
            "item_type": "knowledge",
        }
        result = _render_action(action, {"topic": "file operations"})
        parsed = json.loads(result.split("\n")[1].strip())
        assert parsed["query"] == "file operations"


class TestParserExecuteTags:
    """Parser extracts <execute>, <search>, <load>, <sign> from step XML."""

    def test_parse_execute_tool(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="haiku" />
    <limits turns="3" />
    <permissions><execute>*</execute></permissions>
  </metadata>
  <process>
    <step name="write">
      <description>Write a file</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:output_path}" />
        <param name="content" value="hello" />
      </execute>
    </step>
  </process>
</directive>
```'''
        result = md_parse(md)
        steps = result.get("steps", [])
        assert len(steps) == 1
        assert steps[0]["name"] == "write"
        assert steps[0]["description"] == "Write a file"
        actions = steps[0]["actions"]
        assert len(actions) == 1
        assert actions[0]["primary"] == "execute"
        assert actions[0]["item_type"] == "tool"
        assert actions[0]["item_id"] == "rye/file-system/fs_write"
        assert actions[0]["params"]["path"] == "{input:output_path}"
        assert actions[0]["params"]["content"] == "hello"

    def test_parse_execute_directive(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="haiku" />
    <limits turns="3" />
    <permissions><execute>*</execute></permissions>
  </metadata>
  <process>
    <step name="spawn">
      <description>Run child</description>
      <execute item_type="directive" item_id="child_directive">
        <param name="topic" value="{input:topic}" />
      </execute>
    </step>
  </process>
</directive>
```'''
        result = md_parse(md)
        action = result["steps"][0]["actions"][0]
        assert action["primary"] == "execute"
        assert action["item_type"] == "directive"
        assert action["item_id"] == "child_directive"
        assert action["params"]["topic"] == "{input:topic}"

    def test_parse_search_tag(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="haiku" />
    <limits turns="3" />
    <permissions><search>*</search></permissions>
  </metadata>
  <process>
    <step name="find">
      <description>Find knowledge</description>
      <search item_type="knowledge" query="patterns" />
    </step>
  </process>
</directive>
```'''
        result = md_parse(md)
        action = result["steps"][0]["actions"][0]
        assert action["primary"] == "search"
        assert action["item_type"] == "knowledge"
        assert action["query"] == "patterns"

    def test_parse_load_tag(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="haiku" />
    <limits turns="3" />
    <permissions><load>*</load></permissions>
  </metadata>
  <process>
    <step name="inspect">
      <description>Load directive</description>
      <load item_type="directive" item_id="hello_world" />
    </step>
  </process>
</directive>
```'''
        result = md_parse(md)
        action = result["steps"][0]["actions"][0]
        assert action["primary"] == "load"
        assert action["item_id"] == "hello_world"

    def test_parse_sign_tag(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="haiku" />
    <limits turns="3" />
    <permissions><sign>*</sign></permissions>
  </metadata>
  <process>
    <step name="validate">
      <description>Sign new directive</description>
      <sign item_type="directive" item_id="new_one" />
    </step>
  </process>
</directive>
```'''
        result = md_parse(md)
        action = result["steps"][0]["actions"][0]
        assert action["primary"] == "sign"
        assert action["item_id"] == "new_one"

    def test_parse_multiple_actions_per_step(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="haiku" />
    <limits turns="5" />
    <permissions><execute>*</execute></permissions>
  </metadata>
  <process>
    <step name="multi">
      <description>Write then read</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="/tmp/x" />
        <param name="content" value="data" />
      </execute>
      <execute item_type="tool" item_id="rye/file-system/fs_read">
        <param name="path" value="/tmp/x" />
      </execute>
    </step>
  </process>
</directive>
```'''
        result = md_parse(md)
        actions = result["steps"][0]["actions"]
        assert len(actions) == 2
        assert actions[0]["item_id"] == "rye/file-system/fs_write"
        assert actions[1]["item_id"] == "rye/file-system/fs_read"

    def test_parse_mixed_actions_in_step(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="haiku" />
    <limits turns="5" />
    <permissions><execute>*</execute><search>*</search></permissions>
  </metadata>
  <process>
    <step name="search_then_execute">
      <description>Find then run</description>
      <search item_type="knowledge" query="best practices" />
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="/tmp/report.md" />
        <param name="content" value="report" />
      </execute>
    </step>
  </process>
</directive>
```'''
        result = md_parse(md)
        actions = result["steps"][0]["actions"]
        assert len(actions) == 2
        assert actions[0]["primary"] == "search"
        assert actions[1]["primary"] == "execute"

    def test_step_without_actions_still_works(self):
        md = '''# Test
```xml
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test</description>
    <model tier="haiku" />
    <limits turns="3" />
    <permissions />
  </metadata>
  <process>
    <step name="think">
      <description>Reason about the problem</description>
    </step>
  </process>
</directive>
```'''
        result = md_parse(md)
        step = result["steps"][0]
        assert step["name"] == "think"
        assert step["description"] == "Reason about the problem"
        assert "actions" not in step


class TestFormatStepsWithActions:
    """_format_steps_block renders canonical tool-call JSON from parsed actions."""

    def test_renders_execute_action(self):
        directive = {
            "steps": [{
                "name": "write",
                "description": "Write file",
                "actions": [{
                    "primary": "execute",
                    "item_type": "tool",
                    "item_id": "rye/file-system/fs_write",
                    "params": {"path": "{input:out}", "content": "hello"},
                }],
            }]
        }
        result = _format_steps_block(directive, {"out": "/tmp/test.txt"})
        assert "rye_execute" in result
        assert "/tmp/test.txt" in result
        assert "rye/file-system/fs_write" in result

    def test_renders_search_action(self):
        directive = {
            "steps": [{
                "name": "find",
                "description": "Search",
                "actions": [{"primary": "search", "query": "test", "item_type": "knowledge"}],
            }]
        }
        result = _format_steps_block(directive)
        assert "rye_search" in result

    def test_renders_plain_step_without_actions(self):
        directive = {
            "steps": [{"name": "think", "description": "Reason about it"}]
        }
        result = _format_steps_block(directive)
        assert "- think: Reason about it" in result
        assert "rye_" not in result

    def test_renders_numbered_steps_with_actions(self):
        directive = {
            "steps": [{
                "name": "s1",
                "description": "First",
                "actions": [{
                    "primary": "execute",
                    "item_type": "tool",
                    "item_id": "rye/file-system/fs_read",
                }],
            }, {
                "name": "s2",
                "description": "Second",
                "actions": [{
                    "primary": "execute",
                    "item_type": "tool",
                    "item_id": "rye/file-system/fs_write",
                    "params": {"path": "/tmp/x", "content": "y"},
                }],
            }]
        }
        result = _format_steps_block(directive)
        assert "1) s1" in result
        assert "2) s2" in result
