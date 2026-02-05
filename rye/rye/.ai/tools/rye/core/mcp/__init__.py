# rye:validated:2026-02-05T00:00:00Z:placeholder
"""MCP - connect to any MCP server.

Core MCP tools for discovering and calling external MCP servers.

Tools:
- connect.py: Call MCP tools (HTTP or stdio)
- discover.py: Discover tools from MCP servers
- manager.py: Add/list/refresh/remove MCP servers
"""

from .connect import call_http, call_stdio, execute_with_server_config
from .discover import execute as discover_mcp_tools

__all__ = [
    "call_http",
    "call_stdio",
    "execute_with_server_config",
    "discover_mcp_tools",
]
