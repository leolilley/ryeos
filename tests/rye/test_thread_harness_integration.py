"""Integration tests for thread harness system.

Tests cover:
1. Thread spawn integration - end-to-end directive execution with capability tokens
2. Metadata validation - limits, model, permissions enforced at thread execution time
3. Hooks validation - malformed hooks rejected with clear errors
"""

import importlib.util
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from rye.tools.sign import SignTool

THREAD_DIRECTIVE_PATH = (
    Path(__file__).parent.parent.parent
    / "rye" / "rye" / ".ai" / "tools" / "rye" / "agent" / "threads" / "thread_directive.py"
)
_spec = importlib.util.spec_from_file_location("thread_directive", THREAD_DIRECTIVE_PATH)
thread_directive = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(thread_directive)

TOKENS_PATH = (
    Path(__file__).parent.parent.parent
    / "rye" / "rye" / ".ai" / "tools" / "rye" / "agent" / "capabilities" / "tokens" / "capability_tokens.py"
)
_spec2 = importlib.util.spec_from_file_location("capability_tokens", TOKENS_PATH)
cap_mod = importlib.util.module_from_spec(_spec2)
_spec2.loader.exec_module(cap_mod)

CapabilityToken = cap_mod.CapabilityToken


def _token(caps, hours=1):
    return CapabilityToken(
        caps=caps,
        aud="test",
        exp=datetime.now(timezone.utc) + timedelta(hours=hours),
        directive_id="test",
        thread_id="test-thread",
    )


PRICING_YAML = (
    "models:\n  claude-sonnet-4-20250514:\n    input_per_million: 3.0\n    output_per_million: 15.0\n"
    "default:\n  input_per_million: 5.0\n  output_per_million: 15.0\n"
)


def _write_directive(directives_dir: Path, name: str, xml: str):
    (directives_dir / f"{name}.md").write_text(f"# {name}\n\n```xml\n{xml}\n```\n")


def _make_project(directives: dict[str, str]) -> Path:
    """Create temp project with given directives (name -> xml)."""
    tmpdir = tempfile.mkdtemp()
    root = Path(tmpdir)
    d = root / ".ai" / "directives"
    d.mkdir(parents=True)
    llm = root / ".ai" / "tools" / "llm"
    llm.mkdir(parents=True)
    (llm / "pricing.yaml").write_text(PRICING_YAML)
    for name, xml in directives.items():
        _write_directive(d, name, xml)
    return root


FULL_DIRECTIVE_XML = """\
<directive name="write_hello" version="1.0.0">
  <metadata>
    <description>Write hello to a file</description>
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
    <input name="path" type="string" required="true">Path</input>
  </inputs>
  <process>
    <step name="write">Write hello to {path}</step>
  </process>
  <outputs><success>Done</success></outputs>
</directive>"""

NO_PERMS_DIRECTIVE_XML = """\
<directive name="no_perms" version="1.0.0">
  <metadata>
    <description>Missing permissions declaration</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>3</turns>
      <tokens>1000</tokens>
    </limits>
  </metadata>
  <process><step name="run">Do something</step></process>
</directive>"""

EMPTY_PERMS_DIRECTIVE_XML = """\
<directive name="empty_perms" version="1.0.0">
  <metadata>
    <description>Explicitly declares empty permissions</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>3</turns>
      <tokens>1000</tokens>
    </limits>
    <permissions />
  </metadata>
  <process><step name="run">Do something</step></process>
</directive>"""

NO_LIMITS_XML = """\
<directive name="no_limits" version="1.0.0">
  <metadata>
    <description>Missing limits</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <permissions>
      <execute><tool>rye.file-system.fs_read</tool></execute>
    </permissions>
  </metadata>
  <process><step name="run">Do something</step></process>
</directive>"""

NO_MODEL_XML = """\
<directive name="no_model" version="1.0.0">
  <metadata>
    <description>Missing model</description>
    <category />
    <author>test</author>
    <limits>
      <turns>3</turns>
      <tokens>1000</tokens>
    </limits>
    <permissions />
  </metadata>
  <process><step name="run">Do something</step></process>
</directive>"""

BARE_MINIMUM_XML = """\
<directive name="bare" version="1.0.0">
  <metadata>
    <description>Missing everything</description>
    <category />
    <author>test</author>
  </metadata>
  <process><step name="run">Do something</step></process>
</directive>"""

VALID_HOOKS_XML = """\
<directive name="with_hooks" version="1.0.0">
  <metadata>
    <description>Has valid hooks</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>5</turns>
      <tokens>2000</tokens>
    </limits>
    <permissions>
      <execute><tool>rye.file-system.fs_read</tool></execute>
    </permissions>
    <hooks>
      <hook>
        <when>event.name == 'error'</when>
        <directive>handle_error</directive>
      </hook>
    </hooks>
  </metadata>
  <process><step name="run">Do something</step></process>
</directive>"""

HOOK_MISSING_EVENT_XML = """\
<directive name="hook_no_event" version="1.0.0">
  <metadata>
    <description>Hook missing event/when</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>5</turns>
      <tokens>2000</tokens>
    </limits>
    <permissions />
    <hooks>
      <hook>
        <directive>handle_error</directive>
      </hook>
    </hooks>
  </metadata>
  <process><step name="run">Do something</step></process>
</directive>"""

HOOK_MISSING_DIRECTIVE_XML = """\
<directive name="hook_no_dir" version="1.0.0">
  <metadata>
    <description>Hook missing directive</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>5</turns>
      <tokens>2000</tokens>
    </limits>
    <permissions />
    <hooks>
      <hook>
        <when>event.name == 'limit'</when>
      </hook>
    </hooks>
  </metadata>
  <process><step name="run">Do something</step></process>
</directive>"""

HOOK_MISSING_BOTH_XML = """\
<directive name="hook_empty" version="1.0.0">
  <metadata>
    <description>Hook missing both event and directive</description>
    <category />
    <author>test</author>
    <model tier="general" />
    <limits>
      <turns>5</turns>
      <tokens>2000</tokens>
    </limits>
    <permissions />
    <hooks>
      <hook />
    </hooks>
  </metadata>
  <process><step name="run">Do something</step></process>
</directive>"""


# ---------------------------------------------------------------------------
# 1. Thread spawn integration tests
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
class TestThreadSpawnIntegration:
    """End-to-end thread directive execution with capability tokens."""

    async def test_full_directive_with_matching_token(self):
        """Directive with fs.write + token with fs.write → ready."""
        root = _make_project({"write_hello": FULL_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="write_hello",
                          project_path=str(root), location="project")

        token = _token(["rye.execute.tool.rye.file-system.*"])
        result = await thread_directive.execute(
            directive_name="write_hello",
            inputs={"path": "/tmp/rye_test_output.txt"},
            project_path=str(root),
            _token=token,
            dry_run=True,
        )

        assert result["status"] == "ready"
        assert result["directive"]["name"] == "write_hello"
        assert result["model"]["tier"] == "general"
        assert result["harness"]["limits"]["turns"] == 5

    async def test_permission_denied_without_write(self):
        """Token with only fs.read can't execute fs.write directive."""
        root = _make_project({"write_hello": FULL_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="write_hello",
                          project_path=str(root), location="project")

        token = _token(["rye.execute.tool.rye.file-system.fs_read"])
        result = await thread_directive.execute(
            directive_name="write_hello",
            inputs={"path": "/tmp/test.txt"},
            project_path=str(root),
            _token=token,
        )

        assert result["status"] == "permission_denied"

    async def test_auto_mint_token_no_explicit_token(self):
        """Directive requiring permissions + no token → auto-mint from declared permissions."""
        root = _make_project({"write_hello": FULL_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="write_hello",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="write_hello",
            inputs={"path": "/tmp/test.txt"},
            project_path=str(root),
            dry_run=True,
        )

        assert result["status"] == "ready"

    async def test_empty_perms_no_token_ok(self):
        """Directive with empty <permissions/> works without a token."""
        root = _make_project({"empty_perms": EMPTY_PERMS_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="empty_perms",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="empty_perms",
            project_path=str(root),
            dry_run=True,
        )

        assert result["status"] == "ready"

    async def test_harness_state_serialized(self):
        """Returned harness_state can be used for update_turn."""
        root = _make_project({"empty_perms": EMPTY_PERMS_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="empty_perms",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="empty_perms",
            project_path=str(root),
            dry_run=True,
        )

        state = result["harness_state"]
        assert "cost" in state
        assert "limits" in state
        assert state["limits"]["turns"] == 3

    async def test_directive_not_found(self):
        """Non-existent directive returns clear error."""
        root = _make_project({})
        result = await thread_directive.execute(
            directive_name="nonexistent",
            project_path=str(root),
        )

        assert result["status"] == "failed"
        assert "not found" in result["error"].lower()


# ---------------------------------------------------------------------------
# 2. Metadata validation tests
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
class TestMetadataValidation:
    """Thread execution refuses directives without required metadata."""

    async def test_missing_permissions_rejected(self):
        """Directive without <permissions> block is rejected."""
        root = _make_project({"no_perms": NO_PERMS_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="no_perms",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="no_perms",
            project_path=str(root),
        )

        assert result["status"] == "failed"
        assert "permissions" in result["error"]

    async def test_missing_limits_rejected(self):
        """Directive without <limits> block is rejected."""
        root = _make_project({"no_limits": NO_LIMITS_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="no_limits",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="no_limits",
            project_path=str(root),
        )

        assert result["status"] == "failed"
        assert "limits" in result["error"]

    async def test_missing_model_rejected(self):
        """Directive without <model> tag is rejected."""
        root = _make_project({"no_model": NO_MODEL_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="no_model",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="no_model",
            project_path=str(root),
        )

        assert result["status"] == "failed"
        assert "model" in result["error"]

    async def test_bare_minimum_all_missing(self):
        """Directive missing all three fields lists them all."""
        root = _make_project({"bare": BARE_MINIMUM_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="bare",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="bare",
            project_path=str(root),
        )

        assert result["status"] == "failed"
        assert "limits" in result["error"]
        assert "model" in result["error"]
        assert "permissions" in result["error"]

    async def test_signing_still_works_without_thread_metadata(self):
        """Signing does NOT require limits/model/permissions — only structure."""
        root = _make_project({"bare": BARE_MINIMUM_XML})
        sign = SignTool("")
        result = await sign.handle(
            item_type="directive", item_id="bare",
            project_path=str(root), location="project",
        )

        assert result["status"] == "signed"

    async def test_empty_permissions_accepted(self):
        """<permissions /> (declared but empty) passes validation."""
        root = _make_project({"empty_perms": EMPTY_PERMS_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="empty_perms",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="empty_perms",
            project_path=str(root),
            dry_run=True,
        )

        assert result["status"] == "ready"

    async def test_full_metadata_accepted(self):
        """Directive with all metadata passes validation."""
        root = _make_project({"write_hello": FULL_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="write_hello",
                          project_path=str(root), location="project")

        token = _token(["rye.execute.tool.rye.file-system.*"])
        result = await thread_directive.execute(
            directive_name="write_hello",
            inputs={"path": "/tmp/test.txt"},
            project_path=str(root),
            _token=token,
            dry_run=True,
        )

        assert result["status"] == "ready"


# ---------------------------------------------------------------------------
# 3. Hooks validation tests
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
class TestHooksValidation:
    """Malformed hooks are rejected at thread execution time."""

    async def test_valid_hooks_accepted(self):
        """Well-formed hooks pass validation."""
        root = _make_project({"with_hooks": VALID_HOOKS_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="with_hooks",
                          project_path=str(root), location="project")

        token = _token(["rye.execute.tool.rye.file-system.fs_read"])
        result = await thread_directive.execute(
            directive_name="with_hooks",
            project_path=str(root),
            _token=token,
            dry_run=True,
        )

        assert result["status"] == "ready"
        assert result["harness"]["hooks_count"] == 1

    async def test_hook_missing_event_rejected(self):
        """Hook without event/when field is rejected."""
        root = _make_project({"hook_no_event": HOOK_MISSING_EVENT_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="hook_no_event",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="hook_no_event",
            project_path=str(root),
        )

        assert result["status"] == "failed"
        assert "event" in result["error"] or "when" in result["error"]

    async def test_hook_missing_directive_rejected(self):
        """Hook without directive field is rejected."""
        root = _make_project({"hook_no_dir": HOOK_MISSING_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="hook_no_dir",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="hook_no_dir",
            project_path=str(root),
        )

        assert result["status"] == "failed"
        assert "directive" in result["error"]

    async def test_hook_missing_both_rejected(self):
        """Hook with neither event nor directive is rejected with both errors."""
        root = _make_project({"hook_empty": HOOK_MISSING_BOTH_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="hook_empty",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="hook_empty",
            project_path=str(root),
        )

        assert result["status"] == "failed"
        assert "event" in result["error"] or "when" in result["error"]
        assert "directive" in result["error"]

    async def test_no_hooks_is_fine(self):
        """Directives without hooks pass validation (hooks are optional)."""
        root = _make_project({"empty_perms": EMPTY_PERMS_DIRECTIVE_XML})
        sign = SignTool("")
        await sign.handle(item_type="directive", item_id="empty_perms",
                          project_path=str(root), location="project")

        result = await thread_directive.execute(
            directive_name="empty_perms",
            project_path=str(root),
            dry_run=True,
        )

        assert result["status"] == "ready"
        assert result["harness"]["hooks_count"] == 0
