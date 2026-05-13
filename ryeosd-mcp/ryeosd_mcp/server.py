"""
THREAT MODEL — MCP SERVER

The MCP server is intended for LOCAL SINGLE-USER use:

  * Transport: stdio over a process owned by the operator's OS user.
  * Caller authentication: assumed (the OS user IS the operator).
  * Capability gating: none at the MCP layer — every CLI verb that
    the wrapped `rye` binary exposes is available to any caller
    that reaches the transport.

This means:

  * Do NOT expose the MCP server over the network without a separate
    auth-terminating proxy.
  * Do NOT run the MCP server as a system service available to other
    OS users.
  * DO use it from a single-operator IDE harness (Claude Code,
    Cursor, etc.) on the operator's own machine.

The proper auth design (signed-request from delegated principal,
audience binding, replay protection) is tracked in
`docs/future/mcp-server-auth.md`. It is NOT yet implemented.

See also: `.tmp/STRATEGY-AND-GAPS/02-ENFORCEMENT-GAPS.md` §4.
"""

"""MCP server for RYE OS — thin wrapper over the `rye` CLI.

Exposes a single tool `rye` that shells to the data-driven CLI binary.
Every invocation re-reads `.ai/config/cli/*.yaml`, so new verbs become
callable immediately after bundle install with no MCP redeploy.
"""

import asyncio
import json
import logging
import os
import shutil

from mcp.server import Server
from mcp.server.stdio import stdio_server
from mcp.server.models import InitializationOptions
from mcp.server.lowlevel import NotificationOptions
from mcp.types import Tool, TextContent


logger = logging.getLogger(__name__)

DEFAULT_TIMEOUT_S = 60.0
TOOL_NAME = "cli"
TOOL_DESCRIPTION = (
    "Run any Rye CLI verb. Verbs are loaded data-driven from "
    "`.ai/config/cli/*.yaml` in the bundle hierarchy — call with "
    "`args: [\"help\"]` to list available verbs in the current project. "
    "Pass `project_path` to set the subprocess working directory; the "
    "CLI defaults project_path to `.` when no -p flag is supplied. "
    "Exit code, stdout, and stderr are returned verbatim; if stdout is "
    "valid JSON it is parsed and returned in a `json` field."
)
INPUT_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "properties": {
        "args": {
            "type": "array",
            "items": {"type": "string"},
            "description": (
                "argv passed to `rye`. Do NOT include 'rye' as the first "
                "element — the MCP server prepends the binary path."
            ),
        },
        "project_path": {
            "type": "string",
            "description": (
                "Sets the subprocess cwd. Optional; if omitted the cwd of "
                "the MCP server process is used. The CLI maps cwd to "
                "project_path='.' in the /execute body."
            ),
        },
        "timeout_s": {
            "type": "number",
            "minimum": 1,
            "default": DEFAULT_TIMEOUT_S,
            "description": (
                "Seconds before the MCP kills the subprocess. Default 60. "
                "Long-running graph or knowledge fetches may need a "
                "larger value."
            ),
        },
    },
    "required": ["args"],
}


def _error_result(message: str, error_type: str) -> list[TextContent]:
    """Build a typed JSON error result for user-visible failures.

    Used for input-validation failures and wrapper-originated timeouts.
    Programmer bugs and OS errors are NOT routed here — they propagate
    as MCP-level errors so they cannot masquerade as successful tool
    results.
    """
    return [TextContent(
        type="text",
        text=json.dumps({"error": message, "type": error_type}),
    )]


def _resolve_bin() -> str:
    """Find the `ryeos` binary via RYE_BIN env var or PATH lookup."""
    explicit = os.environ.get("RYE_BIN")
    if explicit:
        return explicit
    # Try new binary name first, then old name for backwards compat
    for name in ("ryeos", "rye"):
        found = shutil.which(name)
        if found:
            return found
    raise RuntimeError(
        "ryeos binary not found on PATH and RYE_BIN not set. "
        "Build via `cargo build --release -p ryeos-cli` and "
        "set RYE_BIN, or add target/release/ to PATH."
    )


async def _run_rye(
    bin_path: str,
    args: list[str],
    project_path: str | None,
    timeout_s: float,
) -> dict:
    """Spawn `rye` as a subprocess and capture its output.

    `stdin` is closed (DEVNULL) so the child can never read from the MCP
    server's stdio pipe — that would corrupt the JSON-RPC protocol on
    stdio transport. `cwd` is forwarded only when explicitly supplied;
    an empty string is rejected by the validation layer rather than
    silently falling through to the server's cwd.
    """
    proc = await asyncio.create_subprocess_exec(
        bin_path,
        *args,
        cwd=project_path if project_path is not None else None,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        out_bytes, err_bytes = await asyncio.wait_for(
            proc.communicate(), timeout=timeout_s,
        )
    except asyncio.TimeoutError:
        proc.kill()
        await proc.wait()
        raise RuntimeError(
            f"rye {args!r} exceeded timeout {timeout_s}s"
        )

    out = out_bytes.decode(errors="replace")
    err = err_bytes.decode(errors="replace")
    result: dict = {
        "exit_code": proc.returncode,
        "stdout": out,
        "stderr": err,
    }
    try:
        result["json"] = json.loads(out)
    except (json.JSONDecodeError, ValueError):
        # stdout was not JSON — that's fine, leave `json` absent.
        pass
    return result


class RYEServer:
    """MCP server that shells to the `rye` CLI."""

    def __init__(self) -> None:
        self._bin = _resolve_bin()
        self.server = Server("rye")
        self._setup()

    def _setup(self) -> None:
        @self.server.list_tools()
        async def list_tools() -> list[Tool]:
            return [
                Tool(
                    name=TOOL_NAME,
                    description=TOOL_DESCRIPTION,
                    inputSchema=INPUT_SCHEMA,
                ),
            ]

        @self.server.call_tool()
        async def call_tool(name: str, arguments: dict) -> list[TextContent]:
            if name != TOOL_NAME:
                return [TextContent(
                    type="text",
                    text=json.dumps({"error": f"unknown tool: {name}"}),
                )]
            # Validation errors (bad input shape) and subprocess
            # timeouts are user-visible failures we surface as JSON
            # tool results. We do NOT catch unexpected exceptions
            # (programmer bugs, OS errors): those must propagate so
            # they show up as MCP-level errors and don't silently
            # masquerade as a "successful" tool result.
            try:
                args = arguments["args"]
            except KeyError:
                return _error_result("missing required field `args`", "ValueError")
            if not isinstance(args, list) or not all(
                isinstance(a, str) for a in args
            ):
                return _error_result(
                    "`args` must be an array of strings", "ValueError"
                )

            project_path = arguments.get("project_path")
            if project_path is not None:
                if not isinstance(project_path, str):
                    return _error_result(
                        "`project_path` must be a string", "ValueError"
                    )
                if project_path == "":
                    return _error_result(
                        "`project_path` must not be empty when supplied; "
                        "omit the field to use the server cwd",
                        "ValueError",
                    )

            raw_timeout = arguments.get("timeout_s", DEFAULT_TIMEOUT_S)
            if not isinstance(raw_timeout, (int, float)) or isinstance(
                raw_timeout, bool
            ):
                return _error_result(
                    "`timeout_s` must be a number", "ValueError"
                )
            timeout_s = float(raw_timeout)
            if timeout_s < 1:
                return _error_result(
                    "`timeout_s` must be >= 1", "ValueError"
                )

            try:
                result = await _run_rye(
                    self._bin, args, project_path, timeout_s,
                )
            except asyncio.TimeoutError as e:
                # _run_rye raises RuntimeError on its own timeout, but
                # if the underlying wait_for surfaces TimeoutError to
                # the caller, treat it as a wrapper-originated failure
                # (no CLI exit code to preserve).
                return _error_result(str(e) or "timeout", "TimeoutError")
            except RuntimeError as e:
                # Includes _run_rye's "exceeded timeout" RuntimeError.
                return _error_result(str(e), "RuntimeError")

            return [TextContent(
                type="text",
                text=json.dumps(result, default=str),
            )]

    async def start(self) -> None:
        async with stdio_server() as (read_stream, write_stream):
            await self.server.run(
                read_stream,
                write_stream,
                InitializationOptions(
                    server_name="rye",
                    server_version="0.2.0",
                    capabilities=self.server.get_capabilities(
                        notification_options=NotificationOptions(),
                        experimental_capabilities={},
                    ),
                ),
            )


async def run_stdio() -> None:
    server = RYEServer()
    await server.start()


def main() -> None:
    asyncio.run(run_stdio())


if __name__ == "__main__":
    main()
