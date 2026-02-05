# rye:validated:2026-02-05T00:00:00Z:placeholder
__tool_type__ = "python"
__version__ = "1.3.0"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/core/mcp"
__tool_description__ = "MCP discover tool - discover tools from MCP servers via stdio, HTTP, or SSE transport"

"""
MCP Discover Tool: Discover tools from an MCP server.

A Python runtime tool that uses the official MCP SDK to connect to MCP servers
and discover available tools. Supports stdio and HTTP (Streamable HTTP) transports.

Transport types:
- "stdio": Local process via stdin/stdout
- "http": Streamable HTTP transport (MCP spec 2025-03-26)
- "sse": Legacy SSE transport (deprecated, use "http" instead)
"""

import asyncio
import json
import logging
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)


def _normalize_schema(schema: Any) -> Optional[Dict[str, Any]]:
    """Normalize inputSchema to dict - handles pydantic models and dicts."""
    if schema is None:
        return None
    if hasattr(schema, "model_dump"):
        return schema.model_dump()
    if isinstance(schema, dict):
        return schema
    return dict(schema)


async def execute(
    transport: str,
    command: Optional[str] = None,
    args: Optional[List[str]] = None,
    env: Optional[Dict[str, str]] = None,
    url: Optional[str] = None,
    headers: Optional[Dict[str, str]] = None,
    auth: Optional[Dict[str, Any]] = None,
    **params,
) -> Dict[str, Any]:
    """
    Discover tools from an MCP server.

    Args:
        transport: Transport type ("stdio", "http", or "sse")
                  - "stdio": Local process via stdin/stdout
                  - "http": Streamable HTTP transport (recommended for remote)
                  - "sse": Legacy SSE transport (deprecated)
        command: Command for stdio transport (required if transport="stdio")
        args: Command arguments for stdio transport
        env: Environment variables for stdio transport
        url: URL for HTTP transport (required if transport="http" or "sse")
        headers: HTTP headers dict (e.g., {"CONTEXT7_API_KEY": "..."})
        auth: Authentication config (legacy, use headers instead)
        **params: Additional parameters

    Returns:
        Result dict with discovered tools
    """
    try:
        # Import MCP SDK
        import httpx
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client
        from mcp.client.streamable_http import streamable_http_client

        tools = []

        if transport == "stdio":
            if not command:
                return {
                    "success": False,
                    "error": "command is required for stdio transport",
                }

            # Create stdio client
            server_params = StdioServerParameters(
                command=command,
                args=args or [],
                env=env or {},
            )

            # Add timeout to prevent stalling
            try:
                async with asyncio.timeout(10):  # 10 second timeout for discovery
                    async with stdio_client(server_params) as (read, write):
                        async with ClientSession(read, write) as session:
                            await session.initialize()

                            # List tools (connection closes automatically after context exits)
                            tools_result = await session.list_tools()
                            for tool in tools_result.tools:
                                input_schema = _normalize_schema(tool.inputSchema)
                                tools.append(
                                    {
                                        "name": tool.name,
                                        "description": tool.description,
                                        "inputSchema": input_schema,
                                    }
                                )
            except asyncio.TimeoutError:
                return {
                    "success": False,
                    "error": "Connection timeout after 10 seconds",
                    "transport": transport,
                    "command": command,
                }

        elif transport in ("http", "sse"):
            # Streamable HTTP transport (MCP spec 2025-03-26)
            # "sse" is accepted for backward compatibility but deprecated
            if not url:
                return {
                    "success": False,
                    "error": f"url is required for {transport} transport",
                }

            # Build headers - prefer explicit headers param, fall back to auth config
            request_headers = dict(headers) if headers else {}

            # Legacy auth config support
            if auth and not request_headers:
                auth_type = auth.get("type", "api_key")

                if auth_type == "bearer":
                    request_headers["Authorization"] = f"Bearer {auth.get('token')}"

                elif auth_type == "api_key":
                    header_name = auth.get("header", "X-API-Key")
                    api_key = auth.get("key")

                    if not api_key:
                        return {
                            "success": False,
                            "error": "auth.key is required when using api_key authentication",
                        }

                    request_headers[header_name] = api_key

            logger.info(f"Connecting to MCP server via Streamable HTTP: {url}")
            logger.info(f"Headers: {list(request_headers.keys())}")

            # Create httpx client with custom headers
            http_client = httpx.AsyncClient(headers=request_headers, timeout=30.0)

            try:
                async with asyncio.timeout(30):
                    async with streamable_http_client(url, http_client=http_client) as (
                        read,
                        write,
                        get_session_id,
                    ):
                        logger.info(
                            "HTTP connection established, initializing session..."
                        )
                        async with ClientSession(read, write) as session:
                            await session.initialize()
                            logger.info("Session initialized, listing tools...")

                            tools_result = await session.list_tools()
                            logger.info(f"Found {len(tools_result.tools)} tools")

                            for tool in tools_result.tools:
                                input_schema = _normalize_schema(tool.inputSchema)
                                tools.append(
                                    {
                                        "name": tool.name,
                                        "description": tool.description,
                                        "inputSchema": input_schema,
                                    }
                                )

            except asyncio.TimeoutError:
                return {
                    "success": False,
                    "error": "Connection timeout after 30 seconds",
                    "transport": "http (streamable)",
                    "url": url,
                    "headers_sent": list(request_headers.keys()),
                    "diagnosis": (
                        "Connection timed out. Possible causes:\n"
                        "1. Wrong URL endpoint\n"
                        "2. Incorrect authentication header name or value\n"
                        "3. Server may be unreachable or not responding\n"
                        "4. Network connectivity issues"
                    ),
                }

            except Exception as e:
                error_msg = str(e)
                error_type = type(e).__name__
                import traceback

                tb_lines = traceback.format_exc().split("\n")
                relevant_tb = tb_lines[-6:-1] if len(tb_lines) > 6 else tb_lines

                return {
                    "success": False,
                    "error": f"{error_type}: {error_msg}",
                    "transport": "http (streamable)",
                    "url": url,
                    "headers_sent": list(request_headers.keys()),
                    "traceback": relevant_tb,
                    "diagnosis": (
                        f"Connection failed with {error_type}. "
                        "Check that the URL is correct and authentication is valid."
                    ),
                }

            finally:
                await http_client.aclose()

        else:
            return {
                "success": False,
                "error": f"Unknown transport: {transport}. Must be 'stdio' or 'http'",
            }

        return {
            "success": True,
            "transport": "stdio" if transport == "stdio" else "http (streamable)",
            "tools": tools,
            "count": len(tools),
        }

    except ImportError as e:
        return {
            "success": False,
            "error": f"MCP SDK not available: {e}",
            "solution": "Install MCP SDK: pip install mcp",
        }

    except Exception as e:
        logger.exception(f"Error discovering MCP tools: {e}")
        import traceback

        return {
            "success": False,
            "error": str(e),
            "error_type": type(e).__name__,
            "transport": transport,
            "traceback": (
                traceback.format_exc() if logger.level <= logging.DEBUG else None
            ),
        }


# CLI entry point for subprocess execution
if __name__ == "__main__":
    import argparse
    import asyncio
    import sys

    parser = argparse.ArgumentParser(description="MCP Discover Tool")
    
    # New unified params mode (used by rye executor)
    parser.add_argument("--params", help="All parameters as JSON")
    parser.add_argument("--project-path", dest="project_path", help="Project path")
    
    # Legacy individual args mode (for direct CLI use)
    parser.add_argument(
        "--transport",
        choices=["stdio", "http", "sse"],
        help="Transport type (http recommended for remote)",
    )
    parser.add_argument("--command", help="Command for stdio transport")
    parser.add_argument("--args", nargs="*", help="Command arguments")
    parser.add_argument("--env", help="Environment variables (JSON)")
    parser.add_argument("--url", help="URL for HTTP transport")
    parser.add_argument(
        "--headers", help='HTTP headers (JSON, e.g., \'{"CONTEXT7_API_KEY": "..."}\')'
    )
    parser.add_argument(
        "--auth", help="Authentication config (JSON, legacy - use --headers instead)"
    )
    parser.add_argument("--debug", action="store_true", help="Enable debug logging")

    args = parser.parse_args()

    # Set up logging
    if args.debug:
        logging.basicConfig(level=logging.DEBUG)
    else:
        logging.basicConfig(level=logging.INFO)

    # Parse params - either from --params JSON or individual args
    if args.params:
        try:
            params = json.loads(args.params)
            transport = params.pop("transport", None)
            if not transport:
                print(json.dumps({"success": False, "error": "transport required in params"}))
                sys.exit(1)
        except json.JSONDecodeError as e:
            print(json.dumps({"success": False, "error": f"Invalid params JSON: {e}"}))
            sys.exit(1)
    else:
        # Legacy mode - build params from individual args
        if not args.transport:
            print(json.dumps({"success": False, "error": "--transport or --params required"}))
            sys.exit(1)
        transport = args.transport
        params = {}
        if args.command:
            params["command"] = args.command
        if args.args:
            params["args"] = args.args
        if args.env:
            params["env"] = json.loads(args.env)
        if args.url:
            params["url"] = args.url
        if args.headers:
            params["headers"] = json.loads(args.headers)
        if args.auth:
            params["auth"] = json.loads(args.auth)

    try:
        result = asyncio.run(execute(transport, **params))
        print(json.dumps(result, indent=2), flush=True)
        sys.stdout.flush()
        sys.exit(0 if result.get("success") else 1)
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}, indent=2), flush=True)
        sys.stdout.flush()
        sys.exit(1)
