# rye:signed:2026-04-06T04:14:25Z:1b76c0eb78a44a58b863392814d4c1b0d502258048274bd25da5e99efc65429e:QyapEuhMX7va5FrQQwCQTc9RNw9nKgYDvmlHtWOiPjEsdxxO-oqrYGT_mHTxShg2TivnH4Fcme9FSMQcu3IFDA:4b987fd4e40303ac
"""
MCP Connect Tool

Executes a tool call on an MCP server via HTTP or stdio transport.
Loads server configuration from YAML, resolves environment variables,
and uses the MCP SDK to make the call.

Usage (with server config):
    echo '{}' | python connect.py --server-config /path/to/server.yaml --tool TOOL_NAME --project-path /path

Usage (direct):
    echo '{}' | python connect.py --transport http --url URL --tool TOOL_NAME [--headers '{}']
"""

__tool_type__ = "python"
__version__ = "2.0.0"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/mcp"
__tool_description__ = (
    "MCP connect tool - execute tool calls on MCP servers via HTTP or stdio"
)

import asyncio
import json
import logging
import os
import re
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional

try:
    from rye.constants import AI_DIR
except ImportError:
    AI_DIR = ".ai"

logger = logging.getLogger(__name__)


def load_dotenv_files(project_path: Optional[Path] = None) -> Dict[str, str]:
    """Load .env files from user space and project."""
    env_vars: Dict[str, str] = {}

    try:
        from dotenv import dotenv_values
    except ImportError:
        logger.debug("python-dotenv not installed, skipping .env loading")
        return env_vars

    user_space = Path.home()

    # Load from user space
    user_env = user_space / AI_DIR / ".env"
    if user_env.exists():
        try:
            loaded = dotenv_values(user_env)
            env_vars.update({k: v for k, v in loaded.items() if v is not None})
        except Exception as e:
            logger.warning(f"Failed to load {user_env}: {e}")

    # Load from project
    if project_path:
        project_path = Path(project_path)
        env_paths = [
            project_path / AI_DIR / ".env",
            project_path / ".env",
            project_path / ".env.local",
        ]
        for env_path in env_paths:
            if env_path.exists():
                try:
                    loaded = dotenv_values(env_path)
                    env_vars.update({k: v for k, v in loaded.items() if v is not None})
                except Exception as e:
                    logger.warning(f"Failed to load {env_path}: {e}")

    return env_vars


def expand_env_vars(value: str, env: Dict[str, str]) -> str:
    """Expand ${VAR} and ${VAR:-default} in value."""
    if not isinstance(value, str):
        return value

    pattern = r"\$\{([^}:]+)(?::-([^}]*))?\}"

    def replacer(match: re.Match[str]) -> str:
        var_name = match.group(1)
        default = match.group(2) or ""
        return env.get(var_name, os.environ.get(var_name, default))

    return re.sub(pattern, replacer, value)


def expand_config(config: Any, env: Dict[str, str]) -> Any:
    """Recursively expand environment variables in config."""
    if isinstance(config, str):
        return expand_env_vars(config, env)
    elif isinstance(config, dict):
        return {k: expand_config(v, env) for k, v in config.items()}
    elif isinstance(config, list):
        return [expand_config(item, env) for item in config]
    return config


def load_server_config(
    config_path: Path, project_path: Optional[Path] = None
) -> Dict[str, Any]:
    """Load and resolve server config from YAML file."""
    import yaml

    if not config_path.exists():
        raise FileNotFoundError(f"Server config not found: {config_path}")

    content = config_path.read_text(encoding="utf-8")

    # Remove signature line if present
    if content.startswith("#"):
        lines = content.split("\n", 1)
        if len(lines) > 1 and "rye:signed:" in lines[0]:
            content = lines[1]

    data = yaml.safe_load(content)
    if not data:
        raise ValueError(f"Empty or invalid server config: {config_path}")

    # Load .env files and merge with os.environ
    dotenv_vars = load_dotenv_files(project_path)
    env = {**os.environ, **dotenv_vars}

    # Expand environment variables in config
    config = data.get("config", {})
    resolved_config = expand_config(config, env)

    return {
        "name": data.get("name", config_path.stem),
        "transport": resolved_config.get("transport", "http"),
        "url": resolved_config.get("url"),
        "headers": resolved_config.get("headers", {}),
        "command": resolved_config.get("command"),
        "args": resolved_config.get("args", []),
        "mcp_server_env": resolved_config.get("env", {}),
        "cwd": resolved_config.get("cwd"),
        "timeout": resolved_config.get("timeout", 30),
    }


async def call_http(
    url: str,
    tool_name: str,
    params: Dict[str, Any],
    headers: Dict[str, str],
    timeout: int = 30,
) -> Dict[str, Any]:
    """Call an MCP tool via HTTP transport."""
    try:
        import httpx
        from mcp import ClientSession
        from mcp.client.streamable_http import streamable_http_client

        http_client = httpx.AsyncClient(headers=headers, timeout=float(timeout))

        try:
            async with asyncio.timeout(timeout):
                async with streamable_http_client(url, http_client=http_client) as (
                    read,
                    write,
                    get_session_id,
                ):
                    async with ClientSession(read, write) as session:
                        await session.initialize()
                        result = await session.call_tool(tool_name, params)
                        return _extract_result(tool_name, result)

        except asyncio.TimeoutError:
            return {
                "success": False,
                "error": f"Timeout after {timeout} seconds",
                "tool": tool_name,
                "url": url,
            }

        finally:
            await http_client.aclose()

    except ImportError as e:
        return {
            "success": False,
            "error": f"MCP SDK not available: {e}",
            "solution": "Install MCP SDK: pip install mcp httpx",
        }

    except Exception as e:
        logger.exception(f"Error calling MCP tool via HTTP: {e}")
        return {
            "success": False,
            "error": str(e),
            "error_type": type(e).__name__,
            "tool": tool_name,
            "url": url,
        }


async def call_stdio(
    command: str,
    args: List[str],
    tool_name: str,
    params: Dict[str, Any],
    mcp_server_env: Optional[Dict[str, str]] = None,
    cwd: Optional[str] = None,
    timeout: int = 30,
) -> Dict[str, Any]:
    """Call an MCP tool via stdio transport."""
    try:
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client

        server_params = StdioServerParameters(
            command=command,
            args=args,
            env=mcp_server_env if mcp_server_env else None,
            cwd=cwd,
        )

        try:
            async with asyncio.timeout(timeout):
                async with stdio_client(server_params) as (read, write):
                    async with ClientSession(read, write) as session:
                        await session.initialize()
                        result = await session.call_tool(tool_name, params)
                        return _extract_result(tool_name, result)

        except asyncio.TimeoutError:
            return {
                "success": False,
                "error": f"Timeout after {timeout} seconds",
                "tool": tool_name,
                "command": f"{command} {' '.join(args)}",
            }

    except ImportError as e:
        return {
            "success": False,
            "error": f"MCP SDK not available: {e}",
            "solution": "Install MCP SDK: pip install mcp",
        }

    except Exception as e:
        logger.exception(f"Error calling MCP tool via stdio: {e}")
        return {
            "success": False,
            "error": str(e),
            "error_type": type(e).__name__,
            "tool": tool_name,
            "command": f"{command} {' '.join(args)}",
        }


_ENVELOPE_KEYS = frozenset({"success", "tool", "content", "isError", "raw", "error", "error_type", "parsed"})


def _try_parse_single_json_content(content_items: list) -> Any:
    """Parse JSON from a single text content block.

    MCP tools typically return structured data as JSON-encoded text inside
    a content block.  When the response is a single text block containing
    valid JSON, parse and return it so callers get structured data directly
    instead of having to manually parse content[0].text.

    Returns the parsed value on success, None otherwise.
    """
    if len(content_items) != 1:
        return None
    block = content_items[0]
    if not isinstance(block, dict) or block.get("type") != "text":
        return None
    text = block.get("text")
    if not isinstance(text, str):
        return None
    text = text.strip()
    if not text:
        return None
    try:
        return json.loads(text)
    except (json.JSONDecodeError, TypeError):
        return None


def _extract_result(tool_name: str, result: Any) -> Dict[str, Any]:
    """Extract content from MCP tool result.

    When the MCP response is a single text content block containing valid
    JSON, the parsed dict keys are promoted to the top level (alongside
    the preserved ``content`` block) so consumers can access fields directly
    — e.g. ``result["email"]["from_email"]`` instead of having to parse
    ``result["content"][0]["text"]``.
    """
    if hasattr(result, "content") and result.content:
        content_items = []
        for item in result.content:
            if hasattr(item, "text"):
                content_items.append({"type": "text", "text": item.text})
            elif hasattr(item, "data"):
                content_items.append({"type": "data", "data": item.data})
            elif hasattr(item, "model_dump"):
                content_items.append(item.model_dump())
            else:
                content_items.append(str(item))

        out = {
            "success": True,
            "tool": tool_name,
            "content": content_items,
            "isError": getattr(result, "isError", False),
        }

        parsed = _try_parse_single_json_content(content_items)
        if isinstance(parsed, dict):
            for k, v in parsed.items():
                if k not in _ENVELOPE_KEYS and k not in out:
                    out[k] = v
            out["parsed"] = parsed
        elif parsed is not None:
            out["parsed"] = parsed

        return out
    else:
        return {
            "success": True,
            "tool": tool_name,
            "content": [],
            "raw": str(result),
        }


async def execute_with_server_config(
    server_config_path: str,
    tool_name: str,
    params: Dict[str, Any],
    project_path: Optional[str] = None,
) -> Dict[str, Any]:
    """Execute MCP tool call using server config file."""
    try:
        config = load_server_config(
            Path(server_config_path),
            Path(project_path) if project_path else None,
        )

        transport = config.get("transport", "http")

        if transport == "http":
            url = config.get("url")
            if not url:
                return {"success": False, "error": "No URL in server config"}

            return await call_http(
                url=url,
                tool_name=tool_name,
                params=params,
                headers=config.get("headers", {}),
                timeout=config.get("timeout", 30),
            )

        elif transport == "stdio":
            command = config.get("command")
            if not command:
                return {"success": False, "error": "No command in server config"}

            return await call_stdio(
                command=command,
                args=config.get("args", []),
                tool_name=tool_name,
                params=params,
                mcp_server_env=config.get("mcp_server_env"),
                cwd=config.get("cwd"),
                timeout=config.get("timeout", 30),
            )

        else:
            return {"success": False, "error": f"Unknown transport: {transport}"}

    except Exception as e:
        return {"success": False, "error": str(e), "error_type": type(e).__name__}


async def execute_direct(
    transport: str,
    tool_name: str,
    params: Dict[str, Any],
    url: Optional[str] = None,
    headers: Optional[Dict[str, str]] = None,
    command: Optional[str] = None,
    args: Optional[List[str]] = None,
    mcp_server_env: Optional[Dict[str, str]] = None,
    cwd: Optional[str] = None,
    timeout: int = 30,
) -> Dict[str, Any]:
    """Execute MCP tool call with direct parameters."""
    if transport == "http":
        if not url:
            return {"success": False, "error": "URL required for HTTP transport"}
        return await call_http(url, tool_name, params, headers or {}, timeout)

    elif transport == "stdio":
        if not command:
            return {"success": False, "error": "Command required for stdio transport"}
        return await call_stdio(command, args or [], tool_name, params, mcp_server_env, cwd, timeout)

    else:
        return {"success": False, "error": f"Unknown transport: {transport}"}


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser(description="MCP Connect Tool")

    # Server config mode
    parser.add_argument(
        "--server-config",
        help="Path to server config YAML file",
    )

    # Direct mode
    parser.add_argument(
        "--transport",
        choices=["http", "stdio"],
        help="Transport type (for direct mode)",
    )
    parser.add_argument("--url", help="MCP server URL (for HTTP)")
    parser.add_argument("--headers", default="{}", help="HTTP headers (JSON)")
    parser.add_argument("--command", help="Command to run (for stdio)")
    parser.add_argument("--args", nargs="*", help="Command arguments (for stdio)")
    parser.add_argument("--env", default="{}", help="Environment variables (JSON)")
    parser.add_argument("--cwd", help="Working directory for stdio transport")

    # Common
    parser.add_argument("--tool", required=True, help="Tool name to call")
    parser.add_argument("--timeout", type=int, default=30, help="Timeout in seconds")
    parser.add_argument("--project-path", help="Project path for .env resolution")
    parser.add_argument("--debug", action="store_true", help="Enable debug logging")

    parsed = parser.parse_args()

    if parsed.debug:
        logging.basicConfig(level=logging.DEBUG)
    else:
        logging.basicConfig(level=logging.INFO)

    try:
        params = json.loads(sys.stdin.read())
    except json.JSONDecodeError as e:
        print(json.dumps({"success": False, "error": f"Invalid params JSON: {e}"}))
        sys.exit(1)

    if parsed.server_config:
        # Server config mode
        result = asyncio.run(
            execute_with_server_config(
                server_config_path=parsed.server_config,
                tool_name=parsed.tool,
                params=params,
                project_path=parsed.project_path,
            )
        )
    elif parsed.transport:
        # Direct mode
        try:
            headers = json.loads(parsed.headers)
            mcp_server_env = json.loads(parsed.env)
        except json.JSONDecodeError as e:
            print(json.dumps({"success": False, "error": f"Invalid JSON: {e}"}))
            sys.exit(1)

        result = asyncio.run(
            execute_direct(
                transport=parsed.transport,
                tool_name=parsed.tool,
                params=params,
                url=parsed.url,
                headers=headers,
                command=parsed.command,
                args=parsed.args,
                mcp_server_env=mcp_server_env,
                cwd=parsed.cwd,
                timeout=parsed.timeout,
            )
        )
    else:
        print(
            json.dumps(
                {"success": False, "error": "Either --server-config or --transport required"}
            )
        )
        sys.exit(1)

    print(json.dumps(result, indent=2), flush=True)
    sys.exit(0 if result.get("success") else 1)
