"""MCP server for RYE OS.

Exposes 4 universal tools:
- mcp__rye__search
- mcp__rye__load
- mcp__rye__execute
- mcp__rye__sign
"""

import asyncio
import json
import logging
import os

from mcp.server import Server
from mcp.server.stdio import stdio_server
from mcp.server.models import InitializationOptions
from mcp.server.lowlevel import NotificationOptions
from mcp.types import Tool, TextContent

from rye.constants import ItemType, Action
from rye.primary_tool_descriptions import (
    EXECUTE_DRY_RUN_DESC,
    EXECUTE_PARAMETERS_DESC,
    EXECUTE_TOOL_DESC,
    ITEM_ID_DESC,
    ITEM_TYPE_DESC,
    LOAD_DESTINATION_DESC,
    LOAD_SOURCE_DESC,
    LOAD_TOOL_DESC,
    PROJECT_PATH_DESC,
    SEARCH_LIMIT_DESC,
    SEARCH_QUERY_DESC,
    SEARCH_SCOPE_DESC,
    SEARCH_SPACE_DESC,
    SEARCH_TOOL_DESC,
    SIGN_ITEM_ID_DESC,
    SIGN_SOURCE_DESC,
    SIGN_TOOL_DESC,
)
from rye.utils.path_utils import get_user_space


logger = logging.getLogger(__name__)


class RYEServer:
    """MCP Server for RYE OS."""

    def __init__(self):
        """Initialize RYE server."""
        self.user_space = str(get_user_space())
        self.debug = os.getenv("RYE_DEBUG", "false").lower() == "true"
        self.server = Server("rye")
        self._setup_handlers()

    def _setup_handlers(self):
        """Register MCP handlers."""
        from rye.tools.search import SearchTool
        from rye.tools.load import LoadTool
        from rye.tools.execute import ExecuteTool
        from rye.tools.sign import SignTool

        self.search = SearchTool(self.user_space)
        self.load = LoadTool(self.user_space)
        self.execute = ExecuteTool(self.user_space)
        self.sign = SignTool(self.user_space)

        @self.server.list_tools()
        async def list_tools() -> list[Tool]:
            """Return 4 MCP tools."""
            return [
                Tool(
                    name="execute",
                    description=EXECUTE_TOOL_DESC,
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "item_type": {
                                "type": "string",
                                "enum": ["directive", "tool", "knowledge"],
                                "description": ITEM_TYPE_DESC,
                            },
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
                            "thread": {
                                "type": "boolean",
                                "default": False,
                                "description": "For directives: spawn a managed thread (LLM loop, safety harness, budgets) instead of returning content in-thread. Default is false (return content for the calling agent to follow).",
                            },
                            "async": {
                                "type": "boolean",
                                "default": False,
                                "description": "For directives (requires thread=true): return immediately with thread_id instead of waiting for completion.",
                            },
                            "model": {
                                "type": "string",
                                "description": "For directives (requires thread=true): override the LLM model used for thread execution.",
                            },
                            "limit_overrides": {
                                "type": "object",
                                "description": "For directives (requires thread=true): override default limits (turns, tokens, spend, spawns, duration_seconds, depth).",
                            },
                        },
                        "required": ["item_type", "item_id", "project_path"],
                    },
                ),
                Tool(
                    name="search",
                    description=SEARCH_TOOL_DESC,
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "scope": {
                                "type": "string",
                                "description": SEARCH_SCOPE_DESC,
                            },
                            "query": {
                                "type": "string",
                                "description": SEARCH_QUERY_DESC,
                            },
                            "project_path": {
                                "type": "string",
                                "description": PROJECT_PATH_DESC,
                            },
                            "space": {
                                "type": "string",
                                "enum": ["project", "user", "system", "all"],
                                "default": "all",
                                "description": SEARCH_SPACE_DESC,
                            },
                            "limit": {
                                "type": "integer",
                                "default": 10,
                                "description": SEARCH_LIMIT_DESC,
                            },
                        },
                        "required": ["scope", "query", "project_path"],
                    },
                ),
                Tool(
                    name="load",
                    description=LOAD_TOOL_DESC,
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "item_type": {
                                "type": "string",
                                "enum": ["directive", "tool", "knowledge"],
                                "description": ITEM_TYPE_DESC,
                            },
                            "item_id": {
                                "type": "string",
                                "description": ITEM_ID_DESC,
                            },
                            "project_path": {
                                "type": "string",
                                "description": PROJECT_PATH_DESC,
                            },
                            "source": {
                                "type": "string",
                                "enum": ["project", "user", "system"],
                                "description": LOAD_SOURCE_DESC,
                            },
                            "destination": {
                                "type": "string",
                                "enum": ["project", "user"],
                                "description": LOAD_DESTINATION_DESC,
                            },
                        },
                        "required": ["item_type", "item_id", "project_path"],
                    },
                ),
                Tool(
                    name="sign",
                    description=SIGN_TOOL_DESC,
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "item_type": {
                                "type": "string",
                                "enum": ItemType.ALL,
                                "description": ITEM_TYPE_DESC,
                            },
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
                        "required": ["item_type", "item_id", "project_path"],
                    },
                ),
            ]

        @self.server.call_tool()
        async def call_tool(name: str, arguments: dict) -> list[TextContent]:
            """Dispatch to appropriate tool."""
            try:
                if name == "search":
                    result = await self.search.handle(**arguments)
                elif name == "load":
                    result = await self.load.handle(**arguments)
                elif name == "execute":
                    result = await self.execute.handle(**arguments)
                elif name == "sign":
                    result = await self.sign.handle(**arguments)
                else:
                    result = {"error": f"Unknown tool: {name}"}

                return [TextContent(type="text", text=json.dumps(result, default=str))]
            except Exception as e:
                import traceback

                error = {"error": str(e), "traceback": traceback.format_exc()}
                return [TextContent(type="text", text=json.dumps(error, indent=2))]

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
    asyncio.run(run_stdio())


if __name__ == "__main__":
    main()
