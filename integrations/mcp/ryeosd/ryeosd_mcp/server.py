"""MCP server for RYE OS — thin wrapper over the `ryeos` CLI.

Exposes a single tool `cli` that shells to the data-driven CLI binary.
Verbs are signed `.ai/node/commands/*.yaml` items in the installed
bundles (e.g. `bundles/codex/.ai/node/commands/`), read by the CLI on
every invocation, so new verbs become callable immediately after bundle
install with no MCP redeploy.

THE MCP SERVER IS A KEY HOLDER

The server acts with a key of its own, never the operator's. The key's file is
named by `RYEOS_CLIENT_KEY` in the server's environment (the server refuses to
start without it) and the server hands that same key to every `ryeos` it
spawns. Whatever the server may do is exactly what that key's grant allows:
the daemon authorizes each request by the key and its capabilities, the way it
does for any other key. A person gives the key a narrowed grant (see the
`mcp-interface` doc); a verb the grant does not cover is refused by the daemon
with a missing-capability error, for whoever holds the key.

  * Transport: stdio over a process the operator starts.
  * Authority: the key's grant, nothing else. The server adds no gating of
    its own and does not look at what is calling it.
  * Commands that act directly on a node's files (key rotation, trust pin,
    project register, resets) need the node's operator key, so `ryeos` refuses
    them for this key with `operator_key_required`.
  * On a node that does not enclose its processes, a process of the same OS
    user can still read the operator's key files. To keep them out of reach,
    run the server as another OS user or inside the enclosure.

The proper auth design (signed-request from delegated principal,
audience binding, replay protection) is tracked in
`.ai/knowledge/ryeos/future/mcp-server-auth.md` at the repository root.
It is NOT yet implemented.
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

from ryeosd_mcp import __version__


logger = logging.getLogger(__name__)

DEFAULT_TIMEOUT_S = 60.0
# Names the key file the CLI signs with. The server holds a key of its own and
# passes it to every `ryeos` it starts.
CLIENT_KEY_ENV = "RYEOS_CLIENT_KEY"
TOOL_NAME = "cli"
TOOL_DESCRIPTION = (
    "Run any Rye CLI verb. Verbs are loaded data-driven from signed "
    "`.ai/node/commands/*.yaml` items in the installed bundles — call with "
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
                "argv passed to `ryeos`. Do NOT include 'ryeos' as the first "
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


def _resolve_client_key() -> str:
    """The server's own key file, from `RYEOS_CLIENT_KEY`.

    There is no fallback: a server without a key of its own would act with
    whatever key the CLI finds by default, which is the operator's.
    """
    key = os.environ.get(CLIENT_KEY_ENV)
    if not key:
        raise RuntimeError(
            f"{CLIENT_KEY_ENV} is not set. The MCP server acts with a key of its "
            "own: create one (`openssl genpkey -algorithm ed25519 -out "
            "mcp-key.pem && chmod 600 mcp-key.pem`), authorize it with a "
            "narrowed grant, and set the variable to its absolute path."
        )
    return key


def _resolve_bin() -> str:
    """Find the `ryeos` binary via RYE_BIN env var or PATH lookup."""
    explicit = os.environ.get("RYE_BIN")
    if explicit:
        return explicit
    found = shutil.which("ryeos")
    if found:
        return found
    raise RuntimeError(
        "ryeos binary not found on PATH and RYE_BIN not set. "
        "Build via `cargo build --release -p ryeos-cli` and "
        "set RYE_BIN, or add target/release/ to PATH."
    )


async def _run_rye(
    bin_path: str,
    client_key: str,
    args: list[str],
    project_path: str | None,
    timeout_s: float,
) -> dict:
    """Spawn `ryeos` as a subprocess and capture its output.

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
        env={**os.environ, CLIENT_KEY_ENV: client_key},
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
            f"ryeos {args!r} exceeded timeout {timeout_s}s"
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
    """MCP server that shells to the `ryeos` CLI."""

    def __init__(self) -> None:
        self._bin = _resolve_bin()
        self._client_key = _resolve_client_key()
        self.server = Server("ryeos")
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
                    self._bin, self._client_key, args, project_path, timeout_s,
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
                    server_name="ryeos",
                    server_version=__version__,
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
