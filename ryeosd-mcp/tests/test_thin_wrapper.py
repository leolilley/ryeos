"""Tests for the thin MCP wrapper — real subprocess, no mocks."""

from __future__ import annotations

import json
import os
import stat
import subprocess
import textwrap

import pytest
from mcp.types import CallToolRequest, CallToolRequestParams, ListToolsRequest

from ryeosd_mcp.server import (
    RYEServer,
    INPUT_SCHEMA,
    TOOL_DESCRIPTION,
    TOOL_NAME,
    _resolve_bin,
)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session")
def rye_bin():
    """Build the `rye` binary once per session if RYE_BIN is not set."""
    explicit = os.environ.get("RYE_BIN")
    if explicit:
        return explicit
    result = subprocess.run(
        ["cargo", "build", "-p", "rye-cli", "--bin", "rye"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        pytest.skip(f"Could not build rye binary: {result.stderr}")
    workspace = os.path.dirname(os.path.dirname(os.path.dirname(__file__)))
    return os.path.join(workspace, "target", "debug", "rye")


@pytest.fixture(autouse=True)
def _env_rye_bin(rye_bin, monkeypatch):
    """Ensure RYE_BIN points at our freshly built binary."""
    monkeypatch.setenv("RYE_BIN", rye_bin)


@pytest.fixture
def project_path():
    """A real project dir with a .ai/ directory (workspace root)."""
    return os.path.dirname(os.path.dirname(os.path.dirname(__file__)))


def _parse_result(server_result):
    """Extract the list of TextContent from a ServerResult."""
    return server_result.root.content


# ---------------------------------------------------------------------------
# 1. list_tools returns exactly one tool named "cli" with expected schema
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_list_tools_single_rye_tool():
    server = RYEServer()
    handler = server.server.request_handlers[ListToolsRequest]
    result = await handler(ListToolsRequest())
    tools = result.root.tools
    assert len(tools) == 1
    tool = tools[0]
    assert tool.name == TOOL_NAME
    assert tool.description == TOOL_DESCRIPTION
    schema = tool.inputSchema
    assert schema["type"] == "object"
    assert schema["additionalProperties"] is False
    assert "args" in schema["required"]
    assert "project_path" not in schema["required"]


# ---------------------------------------------------------------------------
# 2. `rye help` succeeds with exit_code 0 and non-empty stdout
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_help_success(project_path):
    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name=TOOL_NAME,
            arguments={"args": ["help"], "project_path": project_path},
        ),
    ))
    content = _parse_result(result)
    payload = json.loads(content[0].text)
    assert payload["exit_code"] == 0
    assert len(payload["stdout"].strip()) > 0


# ---------------------------------------------------------------------------
# 3. Bad verb returns non-zero exit code with stderr mentioning "unrecognized"
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_bad_verb_nonzero(project_path):
    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name=TOOL_NAME,
            arguments={"args": ["nonexistent-verb-xyz"], "project_path": project_path},
        ),
    ))
    content = _parse_result(result)
    payload = json.loads(content[0].text)
    assert payload["exit_code"] != 0
    assert "unrecognized" in payload["stderr"].lower()


# ---------------------------------------------------------------------------
# 4. additionalProperties: false — extra fields rejected
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_rejects_extra_fields():
    """The MCP library rejects unknown props at the JSON-RPC level with
    an 'Additional properties are not allowed' error before dispatch."""
    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name=TOOL_NAME,
            arguments={"args": ["help"], "extra_field": "x"},
        ),
    ))
    content = _parse_result(result)
    # The MCP schema layer rejects unknown properties and returns a
    # plain-text error, not JSON.
    text = content[0].text
    assert "Additional properties are not allowed" in text


# ---------------------------------------------------------------------------
# 5. Unknown tool name returns an error
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_unknown_name():
    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name="not_rye",
            arguments={"args": ["help"]},
        ),
    ))
    content = _parse_result(result)
    payload = json.loads(content[0].text)
    assert "error" in payload
    assert "unknown tool" in payload["error"]


# ---------------------------------------------------------------------------
# 6. `args` must be an array of strings (type validation)
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_args_type_validation():
    """Passing a string instead of a list for args should produce an error."""
    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name=TOOL_NAME,
            arguments={"args": "help"},
        ),
    ))
    content = _parse_result(result)
    # The MCP schema layer rejects the type mismatch and returns a
    # plain-text error.
    text = content[0].text
    assert "not of type 'array'" in text


# ---------------------------------------------------------------------------
# _resolve_bin (unit)
# ---------------------------------------------------------------------------

def test_resolve_bin_env(monkeypatch):
    monkeypatch.setenv("RYE_BIN", "/custom/rye")
    assert _resolve_bin() == "/custom/rye"


def test_resolve_bin_missing(monkeypatch):
    monkeypatch.delenv("RYE_BIN", raising=False)
    monkeypatch.setenv("PATH", "/dev/null")
    with pytest.raises(RuntimeError, match="rye binary not found"):
        _resolve_bin()


# ---------------------------------------------------------------------------
# Wrapper-specific regression guards.
#
# These tests substitute a tempfile shell script for `rye` so we can
# exercise behavior the real CLI doesn't surface cleanly: timeouts,
# JSON stdout parsing, and cwd propagation. Still real subprocess,
# still no mocks.
# ---------------------------------------------------------------------------

def _make_fake_rye(tmp_path, body: str, name: str = "fake_rye"):
    """Write a `#!/bin/sh` script at tmp_path/<name> and chmod +x.

    `body` is the shell body that runs after the shebang. `name` lets
    callers create multiple distinct scripts in the same tmp_path
    without overwriting each other.
    """
    path = tmp_path / name
    path.write_text("#!/bin/sh\n" + textwrap.dedent(body))
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return str(path)


@pytest.mark.asyncio
async def test_timeout_returns_typed_error(tmp_path, monkeypatch):
    """A subprocess that exceeds timeout_s returns a typed error
    result (NOT a successful exit_code=0 payload, NOT an MCP-level
    crash)."""
    fake = _make_fake_rye(tmp_path, "sleep 5\n")
    monkeypatch.setenv("RYE_BIN", fake)

    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name=TOOL_NAME,
            arguments={"args": ["does-not-matter"], "timeout_s": 1},
        ),
    ))
    payload = json.loads(result.root.content[0].text)
    assert payload.get("type") == "RuntimeError"
    assert "timeout" in payload.get("error", "").lower()
    # Must not look like a successful tool result.
    assert "exit_code" not in payload


@pytest.mark.asyncio
async def test_json_stdout_is_parsed(tmp_path, monkeypatch):
    """When stdout is valid JSON, the wrapper exposes it under `json`.
    When stdout is plain text, `json` is absent."""
    json_fake = _make_fake_rye(
        tmp_path, 'echo \'{"ok": true, "n": 7}\'\n', name="json_rye"
    )
    text_fake = _make_fake_rye(tmp_path, 'echo not json\n', name="text_rye")

    # JSON stdout case
    monkeypatch.setenv("RYE_BIN", json_fake)
    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name=TOOL_NAME,
            arguments={"args": ["any"]},
        ),
    ))
    payload = json.loads(result.root.content[0].text)
    assert payload["exit_code"] == 0
    assert payload["json"] == {"ok": True, "n": 7}

    # Plain-text stdout case
    monkeypatch.setenv("RYE_BIN", text_fake)
    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name=TOOL_NAME,
            arguments={"args": ["any"]},
        ),
    ))
    payload = json.loads(result.root.content[0].text)
    assert payload["exit_code"] == 0
    assert "json" not in payload


@pytest.mark.asyncio
async def test_project_path_is_used_as_cwd(tmp_path, monkeypatch):
    """`project_path` actually sets the subprocess cwd. Without this
    the daemon's view of project_path would silently default."""
    cwd_fake = _make_fake_rye(tmp_path, "pwd\n")
    monkeypatch.setenv("RYE_BIN", cwd_fake)

    target = tmp_path / "subdir"
    target.mkdir()

    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name=TOOL_NAME,
            arguments={"args": ["any"], "project_path": str(target)},
        ),
    ))
    payload = json.loads(result.root.content[0].text)
    assert payload["exit_code"] == 0
    # `pwd` may resolve symlinks (e.g. on macOS /private/var/...);
    # use os.path.realpath on both sides.
    assert os.path.realpath(payload["stdout"].strip()) == os.path.realpath(
        str(target)
    )


@pytest.mark.asyncio
async def test_empty_project_path_is_rejected():
    """An empty-string `project_path` is a typed validation error,
    not a silent fallback to the server cwd."""
    server = RYEServer()
    handler = server.server.request_handlers[CallToolRequest]
    result = await handler(CallToolRequest(
        params=CallToolRequestParams(
            name=TOOL_NAME,
            arguments={"args": ["help"], "project_path": ""},
        ),
    ))
    payload = json.loads(result.root.content[0].text)
    assert payload.get("type") == "ValueError"
    assert "project_path" in payload.get("error", "")


@pytest.mark.asyncio
async def test_exit_code_is_preserved(tmp_path, monkeypatch):
    """CLI exit codes (75 tempfail / 64 usage / 78 config) are
    preserved verbatim — typed signal must reach the agent."""
    for code in (64, 65, 70, 75, 78):
        fake = _make_fake_rye(tmp_path, f"exit {code}\n", name=f"exit_{code}_rye")
        monkeypatch.setenv("RYE_BIN", fake)

        server = RYEServer()
        handler = server.server.request_handlers[CallToolRequest]
        result = await handler(CallToolRequest(
            params=CallToolRequestParams(
                name=TOOL_NAME,
                arguments={"args": ["any"]},
            ),
        ))
        payload = json.loads(result.root.content[0].text)
        assert payload["exit_code"] == code, (
            f"expected exit {code}, got {payload['exit_code']}"
        )
