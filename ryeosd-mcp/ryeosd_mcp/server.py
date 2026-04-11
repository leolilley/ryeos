"""MCP server for RYE OS — daemon-backed execution.

Exposes 3 universal tools: mcp__rye__fetch, mcp__rye__execute, mcp__rye__sign

- execute routes through ryeosd daemon HTTP API (Rust owns control)
- fetch and sign remain Python-direct (Python owns policy: resolution, integrity)
"""

import asyncio
import json
import logging
import os
import urllib.error
import urllib.request

from mcp.server import Server
from mcp.server.stdio import stdio_server
from mcp.server.models import InitializationOptions
from mcp.server.lowlevel import NotificationOptions
from mcp.types import Tool, TextContent

from rye.constants import Action
from rye.primary_action_descriptions import (
    EXECUTE_ASYNC_DESC,
    EXECUTE_DRY_RUN_DESC,
    EXECUTE_PARAMETERS_DESC,
    EXECUTE_RESUME_THREAD_ID_DESC,
    EXECUTE_TARGET_DESC,
    EXECUTE_THREAD_DESC,
    EXECUTE_TOOL_DESC,
    FETCH_DESTINATION_DESC,
    FETCH_LIMIT_DESC,
    FETCH_QUERY_DESC,
    FETCH_SCOPE_DESC,
    FETCH_SOURCE_DESC,
    FETCH_TOOL_DESC,
    ITEM_ID_DESC,
    PROJECT_PATH_DESC,
    SIGN_ITEM_ID_DESC,
    SIGN_SOURCE_DESC,
    SIGN_TOOL_DESC,
)
from rye.utils.path_utils import get_user_space


logger = logging.getLogger(__name__)


def _daemon_url() -> str:
    """Get the daemon base URL from env or default."""
    return os.environ.get("RYEOSD_URL", "http://127.0.0.1:7400")


def _daemon_execute(item_ref: str, parameters: dict = None,
                    launch_mode: str = "inline", model: str = None,
                    budget: dict = None) -> dict:
    """Submit an execution request to the ryeosd daemon via HTTP."""
    payload = {
        "item_ref": item_ref,
        "parameters": parameters or {},
        "launch_mode": launch_mode,
    }
    if model:
        payload["model"] = model
    if budget:
        payload["budget"] = budget

    url = f"{_daemon_url()}/execute"
    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        try:
            err = json.loads(body)
        except json.JSONDecodeError:
            err = {"error": body}
        return {"status": "error", "error": err.get("error", body)}
    except urllib.error.URLError as e:
        return {
            "status": "error",
            "error": f"Cannot connect to ryeosd daemon at {url}: {e.reason}",
        }


class RYEServer:
    """MCP Server for RYE OS — daemon-backed execution."""

    def __init__(self):
        """Initialize RYE server."""
        self.user_space = str(get_user_space())
        self.debug = os.getenv("RYE_DEBUG", "false").lower() == "true"
        self.server = Server("rye")

        from rye.constants import AI_DIR
        os.environ.setdefault("USER_SPACE", self.user_space)
        os.environ.setdefault("AI_DIR", AI_DIR)

        self._setup_handlers()

    def _setup_handlers(self):
        """Register MCP handlers."""
        # fetch and sign remain Python-direct (policy operations)
        from rye.actions.fetch import FetchTool
        from rye.actions.sign import SignTool

        self.fetch = FetchTool(self.user_space)
        self.sign = SignTool(self.user_space)

        @self.server.list_tools()
        async def list_tools() -> list[Tool]:
            """Return 3 MCP tools."""
            return [
                Tool(
                    name="fetch",
                    description=FETCH_TOOL_DESC,
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "item_id": {
                                "type": "string",
                                "description": ITEM_ID_DESC,
                            },
                            "query": {
                                "type": "string",
                                "description": FETCH_QUERY_DESC,
                            },
                            "scope": {
                                "type": "string",
                                "description": FETCH_SCOPE_DESC,
                            },
                            "project_path": {
                                "type": "string",
                                "description": PROJECT_PATH_DESC,
                            },
                            "source": {
                                "type": "string",
                                "enum": ["project", "user", "system", "local", "registry", "all"],
                                "description": FETCH_SOURCE_DESC,
                            },
                            "destination": {
                                "type": "string",
                                "enum": ["project", "user"],
                                "description": FETCH_DESTINATION_DESC,
                            },
                            "version": {
                                "type": "string",
                                "description": "Version to pull (registry source only).",
                            },
                            "limit": {
                                "type": "integer",
                                "default": 10,
                                "description": FETCH_LIMIT_DESC,
                            },
                        },
                        "required": ["project_path"],
                    },
                ),
                Tool(
                    name="execute",
                    description=EXECUTE_TOOL_DESC,
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "item_id": {
                                "type": "string",
                                "description": ITEM_ID_DESC,
                            },
                            "project_path": {
                                "type": "string",
                                "description": PROJECT_PATH_DESC,
                            },
                            "parameters": {
                                "type": "object",
                                "description": EXECUTE_PARAMETERS_DESC,
                            },
                            "dry_run": {
                                "type": "boolean",
                                "default": False,
                                "description": EXECUTE_DRY_RUN_DESC,
                            },
                            "target": {
                                "type": "string",
                                "default": "local",
                                "description": EXECUTE_TARGET_DESC,
                            },
                            "thread": {
                                "type": "string",
                                "enum": ["inline", "fork"],
                                "default": "inline",
                                "description": EXECUTE_THREAD_DESC,
                            },
                            "async": {
                                "type": "boolean",
                                "default": False,
                                "description": EXECUTE_ASYNC_DESC,
                            },
                            "resume_thread_id": {
                                "type": "string",
                                "description": EXECUTE_RESUME_THREAD_ID_DESC,
                            },
                        },
                        "required": ["item_id", "project_path"],
                    },
                ),
                Tool(
                    name="sign",
                    description=SIGN_TOOL_DESC,
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "item_id": {
                                "type": "string",
                                "description": SIGN_ITEM_ID_DESC,
                            },
                            "project_path": {
                                "type": "string",
                                "description": PROJECT_PATH_DESC,
                            },
                            "source": {
                                "type": "string",
                                "enum": ["project", "user"],
                                "default": "project",
                                "description": SIGN_SOURCE_DESC,
                            },
                            "parameters": {"type": "object"},
                        },
                        "required": ["item_id", "project_path"],
                    },
                ),
            ]

        @self.server.call_tool()
        async def call_tool(name: str, arguments: dict) -> list[TextContent]:
            """Dispatch to appropriate tool."""
            try:
                if name == "fetch":
                    result = await self.fetch.handle(**arguments)
                elif name == "execute":
                    result = await self._handle_execute(arguments)
                elif name == "sign":
                    result = await self.sign.handle(**arguments)
                else:
                    result = {"error": f"Unknown tool: {name}"}

                return [TextContent(type="text", text=json.dumps(result, default=str))]
            except Exception as e:
                import traceback

                error = {"error": str(e), "traceback": traceback.format_exc()}
                return [TextContent(type="text", text=json.dumps(error, indent=2))]

    async def _handle_execute(self, arguments: dict) -> dict:
        """Route execute through the ryeosd daemon HTTP API."""
        item_id = arguments.get("item_id", "")
        parameters = arguments.get("parameters", {})

        # Pass through execution flags as parameters
        if arguments.get("dry_run"):
            parameters["dry_run"] = True
        if arguments.get("thread"):
            parameters["thread"] = arguments["thread"]
        if arguments.get("async"):
            parameters["async"] = True
        if arguments.get("resume_thread_id"):
            parameters["resume_thread_id"] = arguments["resume_thread_id"]
        if arguments.get("target") and arguments["target"] != "local":
            parameters["target"] = arguments["target"]

        # Run the HTTP call in a thread to avoid blocking the event loop
        loop = asyncio.get_event_loop()
        result = await loop.run_in_executor(
            None,
            lambda: _daemon_execute(item_id, parameters),
        )
        return result

    async def start(self):
        """Start the MCP server."""
        async with stdio_server() as (read_stream, write_stream):
            await self.server.run(
                read_stream,
                write_stream,
                InitializationOptions(
                    server_name="rye",
                    server_version="0.1.0",
                    capabilities=self.server.get_capabilities(
                        notification_options=NotificationOptions(),
                        experimental_capabilities={},
                    ),
                ),
            )


async def run_stdio():
    """Run in stdio mode."""
    server = RYEServer()
    await server.start()


def main():
    """Entry point."""
    import sys
    os.environ.setdefault("RYE_KERNEL_PYTHON", sys.executable)
    asyncio.run(run_stdio())


if __name__ == "__main__":
    main()
