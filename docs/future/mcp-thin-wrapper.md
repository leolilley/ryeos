# MCP server as a thin wrapper over the `rye` CLI

**Status:** spec, ready to implement.
**Scope:** rewrite of `ryeosd-mcp/` to drop all Python-side policy and
become a single-tool subprocess shell over the data-driven `rye` CLI.
**Stays Python:** yes — the MCP transport library (`mcp`) is Python-native
and the server has no work to do other than spawn a subprocess.

---

## Why

The MCP server today (`ryeosd-mcp/ryeosd_mcp/server.py`, ~325 lines) is
thick:

- Exposes 3 separate MCP tools (`fetch`, `execute`, `sign`) with
  hand-curated input schemas.
- Imports Python helpers from the `rye.*` package
  (`rye.constants`, `rye.primary_action_descriptions`,
  `rye.utils.path_utils`, `rye.actions.fetch`, `rye.actions.sign`).
- Reimplements the daemon HTTP call (`_daemon_execute`) using
  `urllib.request`, parallel to the same logic in the Rust CLI.
- The set of MCP tools is hard-coded — adding a verb means a Python edit
  and a re-deploy.

The new `ryeos-cli` (Rust) is fully data-driven: its verb table is
`.ai/config/cli/*.yaml` resolved across the project / user / system
tiers, and every dispatch goes through HTTP `/execute` to the daemon.
That means **the MCP server has no remaining reason to know any verb
names, schemas, or transport details.** Every additional MCP-side
concept is dead weight that goes stale every time the CLI evolves.

The new shape: **one MCP tool, `rye`, that spawns the `rye` binary as a
subprocess and returns its output.** Because the CLI re-reads the verb
YAMLs on every invocation, new verbs become callable through the MCP
the moment a bundle ships them — no MCP redeploy, no MCP code change.

---

## Architecture

```
AI agent
   │  MCP call: { name: "rye",
   │             arguments: { args: ["status"], project_path: "/x" } }
   ▼
ryeosd-mcp (Python, stdio MCP server)
   │  asyncio.create_subprocess_exec("rye", "status",
   │      cwd="/x", capture stdout/stderr)
   ▼
rye (Rust binary, the data-driven CLI from ryeos-cli/)
   │  reads .ai/config/cli/*.yaml across three tiers
   │  resolves "status" → service:system/status
   │  signs and POSTs /execute to the daemon
   ▼
ryeosd (Rust daemon)
```

The MCP layer's only responsibilities are:

1. Implement the MCP stdio protocol.
2. Spawn `rye` with the supplied argv + cwd.
3. Return `{exit_code, stdout, stderr, json?}` to the agent.

Nothing else.

---

## What gets deleted from `server.py`

Everything except MCP scaffolding and one `subprocess` call:

- `_daemon_url`, `_daemon_execute` — gone (the CLI does this).
- `RYEServer.fetch` / `self.sign` and the `FetchTool` / `SignTool` imports — gone.
- `_handle_execute` — gone (just shell out).
- All per-tool input schemas — replaced by one schema for the `rye` tool.
- All `rye.constants` / `rye.primary_action_descriptions` /
  `rye.utils.path_utils` / `rye.actions.*` imports — gone.

Total resulting `server.py` is ~120 lines.

---

## What gets removed from `pyproject.toml`

The `dependencies = ["ryeos", "mcp"]` shrinks to `["mcp"]`. The MCP no
longer imports any `rye.*` Python modules; it talks to the `rye` binary
on PATH. The optional extras (`web`, `code`, `all`) lose their meaning
when the MCP no longer pulls in `ryeos` — drop them too. Keep `dev`
extras (pytest).

---

## MCP tool surface

### Single tool: `rye`

**Name:** `rye`

**Description (verbatim, this is the prompt the agent reads):**

> Run any Rye CLI verb. Verbs are loaded data-driven from
> `.ai/config/cli/*.yaml` in the bundle hierarchy — call with
> `args: ["help"]` to list available verbs in the current project.
> Pass `project_path` to set the subprocess working directory; the CLI
> defaults `project_path` to `.` when no `-p` flag is supplied. Exit code,
> stdout, and stderr are returned verbatim; if stdout is valid JSON it is
> parsed and returned in a `json` field.

**Input schema (with `additionalProperties: false`):**

```jsonc
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "args": {
      "type":  "array",
      "items": { "type": "string" },
      "description":
        "argv passed to `rye`. First element is the verb token (or 'help', or 'execute' for the universal escape hatch). The MCP server prepends the binary path; do NOT include 'rye' as the first element."
    },
    "project_path": {
      "type":  "string",
      "description":
        "Sets the subprocess cwd. Optional; if omitted the cwd of the MCP server process is used. The CLI maps cwd to project_path='.' in the /execute body."
    },
    "timeout_s": {
      "type":    "number",
      "minimum": 1,
      "default": 60,
      "description":
        "Seconds before the MCP kills the subprocess. Default 60. Long-running graph or knowledge fetches may need a larger value."
    }
  },
  "required": ["args"]
}
```

**Output (always JSON):**

```jsonc
{
  "exit_code": 0,
  "stdout":    "...",   // raw stdout, verbatim
  "stderr":    "...",   // raw stderr, verbatim (CLI warnings included)
  "json":      { ... }  // present only if stdout parsed as JSON; absent otherwise
}
```

### What this means for agents

- Discovering verbs: agent calls `rye(args=["help"])` and reads the
  table in `stdout`. Help is a local-only operation; no daemon
  required.
- Invoking a verb: `rye(args=["status"])`, `rye(args=["thread", "list"])`,
  `rye(args=["execute", "service:system/status"])`, etc.
- Passing parameters: parameters are bound via the CLI's `arg_bind`
  flow, i.e. extra positional/keyword tokens after the verb. e.g.
  `rye(args=["thread", "tail", "T-abc...", "--limit", "20"])`.

The agent never has to know which transport the CLI uses, whether the
daemon is up, or which trust tier owns a verb — the CLI handles all
that.

---

## Reload semantics

There is no in-MCP cache to invalidate. **Every MCP call spawns a fresh
`rye` process**, which re-reads `.ai/config/cli/*.yaml` from the
three-tier hierarchy on every invocation.

So:

1. Operator runs `bundle.install` (or edits a YAML in-tree and signs it).
2. **Next** MCP call automatically sees the new verb. No MCP restart,
   no MCP code change.

If a verb edit silently breaks signature verification, the CLI prints
`rye: warning: skipping <path>: <reason>` to stderr — the MCP forwards
that verbatim, the agent sees it, and the regression is loud.

If you want a dedicated "reload" path that pings the *daemon* (e.g. to
re-read its config), that's already a daemon endpoint
(e.g. `routes.reload`). Exposing it through the MCP just means a verb
YAML pointing `execute: service:routes/reload` — costs zero MCP code.

---

## Behavior decisions (locked in)

These were resolved during spec review — implementation must follow.

### 1. Non-zero exits return JSON, not an MCP error

When `rye` exits non-zero, the MCP returns `{exit_code, stdout, stderr}`
as a successful tool result. The agent decides what to do with the
exit code.

Rationale: the CLI's exit codes are typed and meaningful
(64 = usage / 65 = data error / 70 = software / 75 = tempfail
(daemon down) / 78 = config / 130 = interrupt). Wrapping them in an
MCP-level error throws away that signal. The agent gets richer
information by inspecting `exit_code`.

### 2. No `stdin`, no `env` override fields

- No `stdin`: YAGNI; can be added later if a verb needs it. Keeps the
  surface minimal.
- No `env` override: security smell. The subprocess inherits the
  MCP server's environment; that's the only way to set `RYE_BIN`,
  `RYEOS_STATE_DIR`, `RYE_SYSTEM_SPACE`, `HOME`, etc. The agent
  cannot influence env from inside a tool call.

### 3. `additionalProperties: false`

Unknown fields in the tool input must fail loudly, mirroring the
project rule "typed body deserialization with `deny_unknown_fields`".

### 4. Tool description is verb-agnostic

The description names no specific verbs and contains no embedded
verb list. (Embedding `rye help` output at startup would re-introduce
the staleness problem the redesign exists to eliminate.) The agent
discovers verbs via `args=["help"]` at call time — same source of
truth as the CLI.

### 5. stderr is passed through verbatim

CLI warnings (`rye: warning: skipping ...`, trust verification
failures, signing key issues) appear in the `stderr` field exactly as
the CLI emitted them. No filtering, no normalization. Fail-loud is
the project rule; the MCP must not paper over it.

### 6. Binary discovery: `RYE_BIN` env var, fallback to `which rye`

```python
def _resolve_bin() -> str:
    explicit = os.environ.get("RYE_BIN")
    if explicit:
        return explicit
    found = shutil.which("rye")
    if not found:
        raise RuntimeError(
            "rye binary not found on PATH and RYE_BIN not set. "
            "Build it via `cargo build -p ryeos-cli --bin rye` "
            "and set RYE_BIN, or install ryeos-cli."
        )
    return found
```

Resolution happens once at startup so a missing binary fails before
the first MCP call rather than per call.

---

## Implementation skeleton

This is a faithful sketch — implementer should follow it but is free
to refine the error wording and code organization. The behavior
decisions above are non-negotiable.

```python
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
TOOL_NAME = "rye"
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
            "type":  "array",
            "items": {"type": "string"},
            "description": (
                "argv passed to `rye`. Do NOT include 'rye' as the first "
                "element — the MCP server prepends the binary path."
            ),
        },
        "project_path": {
            "type": "string",
            "description":
                "Sets the subprocess cwd. Optional.",
        },
        "timeout_s": {
            "type":    "number",
            "minimum": 1,
            "default": DEFAULT_TIMEOUT_S,
            "description":
                "Seconds before the subprocess is killed. Default 60.",
        },
    },
    "required": ["args"],
}


def _resolve_bin() -> str:
    explicit = os.environ.get("RYE_BIN")
    if explicit:
        return explicit
    found = shutil.which("rye")
    if not found:
        raise RuntimeError(
            "rye binary not found on PATH and RYE_BIN not set. "
            "Build via `cargo build -p ryeos-cli --bin rye` and "
            "set RYE_BIN, or install ryeos-cli."
        )
    return found


async def _run_rye(
    bin_path: str,
    args: list[str],
    project_path: str | None,
    timeout_s: float,
) -> dict:
    proc = await asyncio.create_subprocess_exec(
        bin_path,
        *args,
        cwd=project_path or None,
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
        "stdout":    out,
        "stderr":    err,
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
            try:
                args = arguments["args"]
                if not isinstance(args, list) or not all(
                    isinstance(a, str) for a in args
                ):
                    raise ValueError("`args` must be an array of strings")
                project_path = arguments.get("project_path")
                if project_path is not None and not isinstance(
                    project_path, str
                ):
                    raise ValueError("`project_path` must be a string")
                timeout_s = float(arguments.get("timeout_s", DEFAULT_TIMEOUT_S))
                if timeout_s < 1:
                    raise ValueError("`timeout_s` must be >= 1")

                result = await _run_rye(
                    self._bin, args, project_path, timeout_s,
                )
                return [TextContent(
                    type="text",
                    text=json.dumps(result, default=str),
                )]
            except Exception as e:
                return [TextContent(
                    type="text",
                    text=json.dumps({
                        "error": str(e),
                        "type":  type(e).__name__,
                    }),
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
```

---

## Tests

One Python test file, `ryeosd-mcp/tests/test_thin_wrapper.py`. No
daemon needed — `rye help` is local-only.

The test:

1. Builds the workspace `rye` binary if absent
   (`cargo build -p ryeos-cli --bin rye`) and sets `RYE_BIN` to the
   path.
2. Spawns the MCP server in stdio mode (or instantiates `RYEServer`
   in-process and exercises its handlers directly — both are
   acceptable; in-process is simpler and faster).
3. Asserts `list_tools()` returns exactly **one** `Tool`, name
   `"rye"`, with the expected schema (presence of `args` required,
   `additionalProperties: false`).
4. Calls the tool with `{"args": ["help"]}`, asserts the result
   is JSON with `exit_code == 0` and a non-empty `stdout`.
5. Calls the tool with `{"args": ["nonexistent-verb-xyz"]}`,
   asserts non-zero `exit_code` and stderr mentioning "unknown verb"
   — i.e. the typed fail-loud is reachable from the MCP.
6. Calls the tool with `{"args": ["help"], "extra_field": "x"}`
   and asserts the schema rejects it (the MCP returns an error or
   the schema layer in the MCP library rejects before dispatch).
   *Implementer's choice on where the rejection lands*; either
   surface is acceptable so long as the unknown field is caught.

No mocking of `subprocess.run` / `create_subprocess_exec`. The whole
point is to exercise the real subprocess path.

Test must run as part of `pytest tests/` from workspace root and not
require a live daemon.

---

## Migration notes

- This change is breaking for any consumer that called `fetch`,
  `execute`, or `sign` directly through the MCP. Migration: call
  the equivalent verb through the single `rye` tool (e.g.
  `rye(args=["fetch", ...])`, `rye(args=["execute", ...])`,
  `rye(args=["sign", ...])` — verb tokens defined by the verb YAMLs
  in `ryeos-bundles/standard/.ai/config/cli/`).
- README / docs that describe the MCP's three-tool surface need a
  one-line update pointing to the single-tool surface and to this
  spec.
- The `ryeosd-mcp` Python package version bumps to **0.2.0** to
  signal the breaking surface change.

---

## What we are explicitly NOT doing

- **Not** keeping a separate `fetch` / `sign` MCP tool for "policy"
  ops. The CLI handles fetch and sign as ordinary verbs; there is no
  policy that the MCP can enforce that the CLI doesn't already.
- **Not** caching or pre-loading the verb list. Caching defeats the
  data-driven point.
- **Not** parsing CLI output for "smart" reformatting. The CLI's
  output is the contract; the MCP forwards it.
- **Not** authenticating MCP-side. Signing happens inside the CLI
  using the user's local key (HOME-based discovery). The MCP shells
  with the same env and inherits that.
- **Not** adding an `env` knob to the tool input. The subprocess
  inherits the MCP process's env; that is the only path.
- **Not** re-introducing in-process Python execution under any flag.
  The CLI is the only path.

---

## Definition of done

- `ryeosd-mcp/ryeosd_mcp/server.py` rewritten as the thin wrapper above.
- `ryeosd-mcp/pyproject.toml` deps reduced to `mcp`; optional extras
  trimmed; version bumped to `0.2.0`.
- `ryeosd-mcp/tests/test_thin_wrapper.py` covering the 6 assertions
  above, passing under `pytest`.
- No imports of `rye.*` Python modules anywhere under
  `ryeosd-mcp/`. No imports of `urllib`. No `_daemon_execute` or
  equivalent HTTP client code.
- `ryeosd-mcp` package installable in isolation (no implicit
  dependency on the deleted Python `ryeos-cli` package).
