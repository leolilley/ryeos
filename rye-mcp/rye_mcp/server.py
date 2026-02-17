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
                    name="search",
                    description="Search for directives, tools, or knowledge by scope",
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "scope": {
                                "type": "string",
                                "description": "Capability-format scope: rye.search.{item_type}.{namespace}.* (e.g., rye.search.directive.rye.core.*) or shorthand: directive, tool.rye.core.*",
                            },
                            "query": {"type": "string"},
                            "project_path": {"type": "string"},
                            "space": {
                                "type": "string",
                                "enum": ["project", "user", "system", "all"],
                                "default": "all",
                            },
                            "limit": {"type": "integer", "default": 10},
                        },
                        "required": ["scope", "query", "project_path"],
                    },
                ),
                Tool(
                    name="load",
                    description="Load item content for inspection or copy between locations",
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "item_type": {
                                "type": "string",
                                "enum": ["directive", "tool", "knowledge"],
                            },
                            "item_id": {"type": "string"},
                            "project_path": {"type": "string"},
                            "source": {
                                "type": "string",
                                "enum": ["project", "user", "system"],
                                "default": "project",
                            },
                            "destination": {
                                "type": "string",
                                "enum": ["project", "user"],
                            },
                        },
                        "required": ["item_type", "item_id", "project_path"],
                    },
                ),
                Tool(
                    name="execute",
                    description="Execute a directive, tool, or knowledge item",
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "item_type": {
                                "type": "string",
                                "enum": ["directive", "tool", "knowledge"],
                            },
                            "item_id": {"type": "string"},
                            "project_path": {"type": "string"},
                            "parameters": {"type": "object"},
                            "dry_run": {"type": "boolean", "default": False},
                        },
                        "required": ["item_type", "item_id", "project_path"],
                    },
                ),
                Tool(
                    name="sign",
                    description="Validate and sign an item file",
                    inputSchema={
                        "type": "object",
                        "properties": {
                            "item_type": {"type": "string", "enum": ItemType.ALL},
                            "item_id": {"type": "string"},
                            "project_path": {"type": "string"},
                            "source": {
                                "type": "string",
                                "enum": ["project", "user"],
                                "default": "project",
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
