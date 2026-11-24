"""Tests for the thin MCP wrapper — real subprocess, no mocks."""

from __future__ import annotations

import json
import os
import stat
import subprocess
import textwrap

import pytest
from mcp import Client
from mcp.types import Implementation, TextContent

from ryeosd_mcp.server import (
    CLIENT_KEY_ENV,
    RYEServer,
    INPUT_SCHEMA,
    TOOL_DESCRIPTION,
    TOOL_NAME,
    _resolve_bin,
    _resolve_client_key,
)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session")
def rye_bin():
    """The real `ryeos` binary, for the few tests that need it.

    `RYE_BIN` names one. Otherwise it is built with cargo, unless
    `RYEOSD_MCP_NO_BUILD=1` says not to (CI runs these tests without a Rust
    build), in which case the test is skipped.
    """
    explicit = os.environ.get("RYE_BIN")
    if explicit:
        return explicit
    if os.environ.get("RYEOSD_MCP_NO_BUILD") == "1":
        pytest.skip("no ryeos binary: set RYE_BIN or unset RYEOSD_MCP_NO_BUILD")
    result = subprocess.run(
        ["cargo", "build", "-p", "ryeos-cli", "--bin", "ryeos"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        pytest.skip(f"Could not build ryeos binary: {result.stderr}")
    workspace = os.path.dirname(os.path.dirname(os.path.dirname(__file__)))
    return os.path.join(workspace, "target", "debug", "ryeos")


@pytest.fixture
def real_rye_bin(rye_bin, monkeypatch):
    """Point `RYE_BIN` at the real binary for one test."""
    monkeypatch.setenv("RYE_BIN", rye_bin)
    return rye_bin


@pytest.fixture(autouse=True)
def _fake_rye_by_default(tmp_path, monkeypatch):
    """Every test starts with a harmless `ryeos` so the server can be built;
    tests that need the real binary ask for `real_rye_bin`."""
    path = tmp_path / "default_rye"
    path.write_text("#!/bin/sh\nexit 0\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    monkeypatch.setenv("RYE_BIN", str(path))


@pytest.fixture(autouse=True)
def _env_client_key(tmp_path_factory, monkeypatch):
    """The server holds a key of its own; give every test one (owner-only)."""
    key = tmp_path_factory.mktemp("mcp-key") / "mcp-key.pem"
    key.write_text("not a real key; only the path matters to these tests")
    key.chmod(0o600)
    monkeypatch.setenv("RYEOS_CLIENT_KEY", str(key))
    return str(key)


@pytest.fixture
def project_path():
    """A real project dir with a .ai/ directory (workspace root)."""
    return os.path.dirname(os.path.dirname(os.path.dirname(__file__)))


async def _call(server, name, arguments, **client_kwargs):
    """Call a tool over an in-process client, the way a client would."""
    async with Client(server.server, **client_kwargs) as client:
        return await client.call_tool(name, arguments)


def _payload(result):
    """The JSON the wrapper put in the result's one text block."""
    block = result.content[0]
    assert isinstance(block, TextContent)
    return json.loads(block.text)


# ---------------------------------------------------------------------------
# 1. list_tools returns exactly one tool named "cli" with expected schema
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_list_tools_single_rye_tool():
    server = RYEServer()
    async with Client(server.server) as client:
        result = await client.list_tools()
    tools = result.tools
    assert len(tools) == 1
    tool = tools[0]
    assert tool.name == TOOL_NAME
    assert tool.description == TOOL_DESCRIPTION
    schema = tool.input_schema
    assert schema["type"] == "object"
    assert schema["additionalProperties"] is False
    assert "args" in schema["required"]
    assert "project_path" not in schema["required"]


# ---------------------------------------------------------------------------
# 2. `rye help` succeeds with exit_code 0 and non-empty stdout
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_help_success(project_path, real_rye_bin):
    server = RYEServer()
    result = await _call(server, TOOL_NAME, {"args": ["help"], "project_path": project_path})
    payload = _payload(result)
    assert payload["exit_code"] == 0
    assert len(payload["stdout"].strip()) > 0


# ---------------------------------------------------------------------------
# 3. Bad verb returns non-zero exit code with stderr mentioning "unrecognized"
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_bad_verb_nonzero(project_path, real_rye_bin):
    server = RYEServer()
    result = await _call(server, TOOL_NAME, {"args": ["nonexistent-verb-xyz"], "project_path": project_path})
    payload = _payload(result)
    assert payload["exit_code"] != 0
    assert "unrecognized" in payload["stderr"].lower()


# ---------------------------------------------------------------------------
# 4. additionalProperties: false — extra fields rejected
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_rejects_extra_fields():
    """The v2 SDK does not check arguments against the schema, so the
    wrapper enforces `additionalProperties: false` itself, as a typed error."""
    server = RYEServer()
    result = await _call(server, TOOL_NAME, {"args": ["help"], "extra_field": "x"})
    payload = _payload(result)
    assert payload["type"] == "ValueError"
    assert "extra_field" in payload["error"]


# ---------------------------------------------------------------------------
# 5. Unknown tool name returns an error
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_unknown_name():
    server = RYEServer()
    result = await _call(server, "not_rye", {"args": ["help"]})
    payload = _payload(result)
    assert "error" in payload
    assert "unknown tool" in payload["error"]


# ---------------------------------------------------------------------------
# 6. `args` must be an array of strings (type validation)
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_call_tool_args_type_validation():
    """Passing a string instead of a list for args should produce an error."""
    server = RYEServer()
    result = await _call(server, TOOL_NAME, {"args": "help"})
    payload = _payload(result)
    assert payload["type"] == "ValueError"
    assert "array of strings" in payload["error"]


# ---------------------------------------------------------------------------
# _resolve_bin (unit)
# ---------------------------------------------------------------------------

def test_resolve_bin_env(monkeypatch):
    monkeypatch.setenv("RYE_BIN", "/custom/ryeos")
    assert _resolve_bin() == "/custom/ryeos"


def test_resolve_bin_missing(monkeypatch):
    monkeypatch.delenv("RYE_BIN", raising=False)
    monkeypatch.setenv("PATH", "/dev/null")
    with pytest.raises(RuntimeError, match="ryeos binary not found"):
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
    result = await _call(server, TOOL_NAME, {"args": ["does-not-matter"], "timeout_s": 1})
    payload = json.loads(result.content[0].text)
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
    result = await _call(server, TOOL_NAME, {"args": ["any"]})
    payload = json.loads(result.content[0].text)
    assert payload["exit_code"] == 0
    assert payload["json"] == {"ok": True, "n": 7}

    # Plain-text stdout case
    monkeypatch.setenv("RYE_BIN", text_fake)
    server = RYEServer()
    result = await _call(server, TOOL_NAME, {"args": ["any"]})
    payload = json.loads(result.content[0].text)
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
    result = await _call(server, TOOL_NAME, {"args": ["any"], "project_path": str(target)})
    payload = json.loads(result.content[0].text)
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
    result = await _call(server, TOOL_NAME, {"args": ["help"], "project_path": ""})
    payload = json.loads(result.content[0].text)
    assert payload.get("type") == "ValueError"
    assert "project_path" in payload.get("error", "")


@pytest.mark.asyncio
async def test_exit_code_is_preserved(tmp_path, monkeypatch):
    """CLI exit codes (75 tempfail / 64 usage / 78 config) are
    preserved verbatim — the typed signal must reach the caller."""
    for code in (64, 65, 70, 75, 78):
        fake = _make_fake_rye(tmp_path, f"exit {code}\n", name=f"exit_{code}_rye")
        monkeypatch.setenv("RYE_BIN", fake)

        server = RYEServer()
        result = await _call(server, TOOL_NAME, {"args": ["any"]})
        payload = json.loads(result.content[0].text)
        assert payload["exit_code"] == code, (
            f"expected exit {code}, got {payload['exit_code']}"
        )


def test_the_server_refuses_to_start_without_a_key_of_its_own(monkeypatch):
    """No fallback to whatever key the CLI finds by default (the operator's)."""
    monkeypatch.delenv("RYEOS_CLIENT_KEY", raising=False)
    with pytest.raises(RuntimeError, match="RYEOS_CLIENT_KEY is not set"):
        _resolve_client_key()
    monkeypatch.setenv("RYEOS_CLIENT_KEY", "")
    with pytest.raises(RuntimeError, match="RYEOS_CLIENT_KEY is not set"):
        RYEServer()


@pytest.mark.asyncio
async def test_every_call_carries_the_servers_own_key(
    tmp_path, monkeypatch, _env_client_key
):
    """The key the server holds is handed to each `ryeos` it starts."""
    fake = _make_fake_rye(tmp_path, 'echo "key=$RYEOS_CLIENT_KEY"\n')
    monkeypatch.setenv("RYE_BIN", fake)
    server = RYEServer()
    result = await _call(server, TOOL_NAME, {"args": ["endorse"]})
    payload = json.loads(result.content[0].text)
    assert payload["stdout"].strip() == f"key={_env_client_key}"


@pytest.mark.asyncio
@pytest.mark.parametrize("mode", ["auto", "legacy"])
async def test_both_protocol_eras_are_served(mode):
    """The SDK serves the stateless era (`server/discover`, per-request
    metadata) and the legacy `initialize` handshake from one server."""
    server = RYEServer()
    async with Client(server.server, mode=mode) as client:
        assert client.server_info.name == "ryeos"
        assert client.server_info.version == server_version()
        assert client.protocol_version == (
            "2026-07-28" if mode == "auto" else "2025-11-25"
        )
        tools = await client.list_tools()
        assert [tool.name for tool in tools.tools] == [TOOL_NAME]
        result = await client.call_tool(TOOL_NAME, {"args": ["any"]})
        assert result.result_type == "complete"
        assert _payload(result)["exit_code"] == 0


@pytest.mark.asyncio
async def test_what_a_client_says_about_itself_decides_nothing():
    """`clientInfo` is display only: two clients that name themselves
    differently get the same answer to the same call."""
    server = RYEServer()
    answers = []
    for name in ("first-client", "a-different-client"):
        result = await _call(
            server,
            TOOL_NAME,
            {"args": ["any"]},
            client_info=Implementation(name=name, version="9.9"),
        )
        answers.append(_payload(result))
    assert answers[0] == answers[1]


def server_version():
    from ryeosd_mcp import __version__

    return __version__


def test_server_version_matches_package():
    """The MCP server reports the package version, kept in one place
    in `ryeosd_mcp/__init__.py` and matching `pyproject.toml`."""
    import tomllib

    import ryeosd_mcp
    from ryeosd_mcp import server

    assert server.__version__ == ryeosd_mcp.__version__
    pyproject = os.path.join(os.path.dirname(os.path.dirname(__file__)), "pyproject.toml")
    with open(pyproject, "rb") as f:
        assert tomllib.load(f)["project"]["version"] == ryeosd_mcp.__version__
