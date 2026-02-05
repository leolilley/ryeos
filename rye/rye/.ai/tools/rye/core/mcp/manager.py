# rye:validated:2026-02-04T23:40:35Z:2dc018235a9a5a090561246388739b578ae574f69868ae8ed27eb22beeca6be7
"""
MCP Manager Tool

Manages MCP server configurations and discovered tools.
Actions: add, list, refresh, remove

Usage:
    python manager.py --action add --name context7 --transport http --url URL [--headers '{}'] --project-path /path
    python manager.py --action list --project-path /path [--include-tools]
    python manager.py --action refresh --name context7 --project-path /path
    python manager.py --action remove --name context7 --project-path /path
"""

__tool_type__ = "python"
__version__ = "1.0.0"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/core/mcp"
__tool_description__ = "MCP manager - add, list, refresh, and remove MCP server configurations"

import asyncio
import json
import logging
import os
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)


def get_mcp_servers_dir(project_path: Path, scope: str = "project") -> Path:
    """Get the MCP servers directory for the given scope."""
    if scope == "user":
        return Path.home() / ".ai" / "tools" / "mcp" / "servers"
    else:
        return project_path / ".ai" / "tools" / "mcp" / "servers"


def get_mcp_tools_dir(project_path: Path, server_name: str, scope: str = "project") -> Path:
    """Get the MCP tools directory for a server."""
    if scope == "user":
        return Path.home() / ".ai" / "tools" / "mcp" / server_name
    else:
        return project_path / ".ai" / "tools" / "mcp" / server_name


def generate_signature_placeholder() -> str:
    """Generate a placeholder signature line."""
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    return f"# rye:validated:{timestamp}:placeholder"


def create_server_config(
    name: str,
    transport: str,
    url: Optional[str] = None,
    headers: Optional[Dict[str, str]] = None,
    command: Optional[str] = None,
    args: Optional[List[str]] = None,
    env: Optional[Dict[str, str]] = None,
    timeout: int = 30,
) -> str:
    """Generate server config YAML content."""
    import yaml

    config: Dict[str, Any] = {
        "transport": transport,
        "timeout": timeout,
    }

    if transport == "http":
        if url:
            config["url"] = url
        if headers:
            config["headers"] = headers
    elif transport == "stdio":
        if command:
            config["command"] = command
        if args:
            config["args"] = args
        if env:
            config["env"] = env

    data = {
        "tool_type": "mcp_server",
        "executor_id": None,
        "category": "mcp/servers",
        "version": "1.0.0",
        "description": f"MCP server: {name}",
        "config": config,
        "cache": {
            "discovered_at": None,
            "tool_count": 0,
        },
    }

    yaml_content = yaml.dump(data, default_flow_style=False, sort_keys=False)
    return f"{generate_signature_placeholder()}\n{yaml_content}"


def create_tool_config(
    server_name: str,
    tool_name: str,
    description: str,
    input_schema: Optional[Dict[str, Any]] = None,
    transport: str = "http",
    scope: str = "project",
) -> str:
    """Generate tool config YAML content.
    
    Stores server_config_path as a template for data-driven execution.
    The path template uses {project_path} or {user_space} based on scope.
    """
    import yaml

    runtime = (
        "rye/core/runtimes/mcp_http_runtime"
        if transport == "http"
        else "rye/core/runtimes/mcp_stdio_runtime"
    )

    # Build server config path template based on scope
    if scope == "user":
        server_config_path = "{user_space}/tools/mcp/servers/" + server_name + ".yaml"
    else:
        server_config_path = "{project_path}/.ai/tools/mcp/servers/" + server_name + ".yaml"

    data = {
        "tool_type": "mcp",
        "executor_id": runtime,
        "category": f"mcp/{server_name}",
        "version": "1.0.0",
        "description": description or f"MCP tool: {tool_name}",
        "config": {
            "server": f"mcp/servers/{server_name}",  # Informational
            "server_config_path": server_config_path,  # Used for execution
            "tool_name": tool_name,
        },
    }

    if input_schema:
        data["input_schema"] = input_schema

    yaml_content = yaml.dump(data, default_flow_style=False, sort_keys=False)
    return f"{generate_signature_placeholder()}\n{yaml_content}"


async def discover_tools(
    transport: str,
    url: Optional[str] = None,
    headers: Optional[Dict[str, str]] = None,
    command: Optional[str] = None,
    args: Optional[List[str]] = None,
    env: Optional[Dict[str, str]] = None,
    timeout: int = 30,
) -> Dict[str, Any]:
    """Discover tools from an MCP server."""
    # Import discover module - handle both package and direct execution
    try:
        from . import discover
    except ImportError:
        # Direct execution - import from same directory
        import importlib.util
        discover_path = Path(__file__).parent / "discover.py"
        spec = importlib.util.spec_from_file_location("discover", discover_path)
        if not spec or not spec.loader:
            return {"success": False, "error": "Could not load discover module"}
        discover = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(discover)

    if transport == "http":
        return await discover.execute(
            transport="http",
            url=url,
            headers=headers,
            timeout=timeout,
        )
    elif transport == "stdio":
        return await discover.execute(
            transport="stdio",
            command=command,
            args=args,
            env=env,
            timeout=timeout,
        )
    else:
        return {"success": False, "error": f"Unknown transport: {transport}"}


async def action_add(
    name: str,
    transport: str,
    project_path: Path,
    scope: str = "project",
    url: Optional[str] = None,
    headers: Optional[Dict[str, str]] = None,
    command: Optional[str] = None,
    args: Optional[List[str]] = None,
    env: Optional[Dict[str, str]] = None,
    timeout: int = 30,
) -> Dict[str, Any]:
    """Add a new MCP server and discover its tools."""
    servers_dir = get_mcp_servers_dir(project_path, scope)
    tools_dir = get_mcp_tools_dir(project_path, name, scope)

    # Check if already exists
    server_file = servers_dir / f"{name}.yaml"
    if server_file.exists():
        return {
            "success": False,
            "error": f"Server '{name}' already exists at {server_file}",
        }

    # Discover tools first
    logger.info(f"Discovering tools from {name}...")
    discover_result = await discover_tools(
        transport=transport,
        url=url,
        headers=headers,
        command=command,
        args=args,
        env=env,
        timeout=timeout,
    )

    if not discover_result.get("success"):
        return {
            "success": False,
            "error": f"Failed to discover tools: {discover_result.get('error')}",
            "discovery_result": discover_result,
        }

    tools = discover_result.get("tools", [])
    logger.info(f"Discovered {len(tools)} tools")

    # Create directories
    servers_dir.mkdir(parents=True, exist_ok=True)
    tools_dir.mkdir(parents=True, exist_ok=True)

    # Write server config
    server_content = create_server_config(
        name=name,
        transport=transport,
        url=url,
        headers=headers,
        command=command,
        args=args,
        env=env,
        timeout=timeout,
    )
    server_file.write_text(server_content, encoding="utf-8")
    logger.info(f"Created server config: {server_file}")

    # Write tool configs
    created_tools = []
    for tool in tools:
        tool_name = tool.get("name")
        if not tool_name:
            continue

        tool_content = create_tool_config(
            server_name=name,
            tool_name=tool_name,
            description=tool.get("description", ""),
            input_schema=tool.get("inputSchema"),
            transport=transport,
            scope=scope,
        )

        # Sanitize tool name for filename
        safe_name = tool_name.replace("/", "_").replace("\\", "_")
        tool_file = tools_dir / f"{safe_name}.yaml"
        tool_file.write_text(tool_content, encoding="utf-8")
        created_tools.append(tool_name)
        logger.info(f"Created tool config: {tool_file}")

    # Update server config with cache info
    import yaml

    server_data = yaml.safe_load(server_file.read_text().split("\n", 1)[1])
    server_data["cache"] = {
        "discovered_at": datetime.now(timezone.utc).isoformat(),
        "tool_count": len(created_tools),
    }
    updated_content = f"{generate_signature_placeholder()}\n{yaml.dump(server_data, default_flow_style=False, sort_keys=False)}"
    server_file.write_text(updated_content, encoding="utf-8")

    return {
        "success": True,
        "server": name,
        "server_config": str(server_file),
        "tools_dir": str(tools_dir),
        "tools_created": created_tools,
        "tool_count": len(created_tools),
    }


async def action_list(
    project_path: Path,
    include_tools: bool = False,
) -> Dict[str, Any]:
    """List all configured MCP servers."""
    import yaml

    servers = []

    # Search in project and user space
    search_paths = [
        ("project", project_path / ".ai" / "tools" / "mcp" / "servers"),
        ("user", Path.home() / ".ai" / "tools" / "mcp" / "servers"),
    ]

    for scope, servers_dir in search_paths:
        if not servers_dir.exists():
            continue

        for server_file in servers_dir.glob("*.yaml"):
            try:
                content = server_file.read_text(encoding="utf-8")
                # Skip signature line
                if content.startswith("#"):
                    content = content.split("\n", 1)[1]
                data = yaml.safe_load(content)

                server_info = {
                    "name": server_file.stem,
                    "scope": scope,
                    "path": str(server_file),
                    "transport": data.get("config", {}).get("transport", "unknown"),
                    "url": data.get("config", {}).get("url"),
                    "command": data.get("config", {}).get("command"),
                    "cache": data.get("cache", {}),
                }

                if include_tools:
                    tools_dir = server_file.parent.parent / server_file.stem
                    if tools_dir.exists():
                        tool_names = [f.stem for f in tools_dir.glob("*.yaml")]
                        server_info["tools"] = tool_names
                    else:
                        server_info["tools"] = []

                servers.append(server_info)

            except Exception as e:
                logger.warning(f"Failed to load server config {server_file}: {e}")

    return {
        "success": True,
        "servers": servers,
        "count": len(servers),
    }


async def action_refresh(
    name: str,
    project_path: Path,
) -> Dict[str, Any]:
    """Refresh tools for an existing MCP server."""
    import yaml

    # Find the server config
    server_file = None
    scope = None

    for s, servers_dir in [
        ("project", project_path / ".ai" / "tools" / "mcp" / "servers"),
        ("user", Path.home() / ".ai" / "tools" / "mcp" / "servers"),
    ]:
        candidate = servers_dir / f"{name}.yaml"
        if candidate.exists():
            server_file = candidate
            scope = s
            break

    if not server_file:
        return {"success": False, "error": f"Server '{name}' not found"}

    # Load server config
    content = server_file.read_text(encoding="utf-8")
    if content.startswith("#"):
        content = content.split("\n", 1)[1]
    data = yaml.safe_load(content)
    config = data.get("config", {})

    # Discover tools
    discover_result = await discover_tools(
        transport=config.get("transport", "http"),
        url=config.get("url"),
        headers=config.get("headers"),
        command=config.get("command"),
        args=config.get("args"),
        env=config.get("env"),
        timeout=config.get("timeout", 30),
    )

    if not discover_result.get("success"):
        return {
            "success": False,
            "error": f"Failed to discover tools: {discover_result.get('error')}",
        }

    tools = discover_result.get("tools", [])
    tools_dir = get_mcp_tools_dir(project_path, name, scope or "project")

    # Remove old tool configs
    if tools_dir.exists():
        for old_file in tools_dir.glob("*.yaml"):
            old_file.unlink()

    # Create new tool configs
    tools_dir.mkdir(parents=True, exist_ok=True)
    created_tools = []

    for tool in tools:
        tool_name = tool.get("name")
        if not tool_name:
            continue

        tool_content = create_tool_config(
            server_name=name,
            tool_name=tool_name,
            description=tool.get("description", ""),
            input_schema=tool.get("inputSchema"),
            transport=config.get("transport", "http"),
            scope=scope or "project",
        )

        safe_name = tool_name.replace("/", "_").replace("\\", "_")
        tool_file = tools_dir / f"{safe_name}.yaml"
        tool_file.write_text(tool_content, encoding="utf-8")
        created_tools.append(tool_name)

    # Update cache in server config
    data["cache"] = {
        "discovered_at": datetime.now(timezone.utc).isoformat(),
        "tool_count": len(created_tools),
    }
    updated_content = f"{generate_signature_placeholder()}\n{yaml.dump(data, default_flow_style=False, sort_keys=False)}"
    server_file.write_text(updated_content, encoding="utf-8")

    return {
        "success": True,
        "server": name,
        "tools_refreshed": created_tools,
        "tool_count": len(created_tools),
    }


async def action_remove(
    name: str,
    project_path: Path,
) -> Dict[str, Any]:
    """Remove an MCP server and its tools."""
    removed = []

    for scope, base_dir in [
        ("project", project_path / ".ai" / "tools" / "mcp"),
        ("user", Path.home() / ".ai" / "tools" / "mcp"),
    ]:
        server_file = base_dir / "servers" / f"{name}.yaml"
        tools_dir = base_dir / name

        if server_file.exists():
            server_file.unlink()
            removed.append(str(server_file))

        if tools_dir.exists():
            shutil.rmtree(tools_dir)
            removed.append(str(tools_dir))

    if not removed:
        return {"success": False, "error": f"Server '{name}' not found"}

    return {
        "success": True,
        "server": name,
        "removed": removed,
    }


async def execute_action(
    action: str,
    project_path: Path,
    params: Dict[str, Any],
) -> Dict[str, Any]:
    """Execute an MCP manager action.
    
    Args:
        action: One of "add", "list", "refresh", "remove"
        project_path: Project root path
        params: Action-specific parameters
        
    Returns:
        Result dict
    """
    if action == "add":
        name = params.get("name")
        transport = params.get("transport")
        if not name or not transport:
            return {"success": False, "error": "name and transport required for add"}

        return await action_add(
            name=name,
            transport=transport,
            project_path=project_path,
            scope=params.get("scope", "project"),
            url=params.get("url"),
            headers=params.get("headers"),
            command=params.get("command"),
            args=params.get("args"),
            env=params.get("env"),
            timeout=params.get("timeout", 30),
        )

    elif action == "list":
        return await action_list(
            project_path=project_path,
            include_tools=params.get("include_tools", False),
        )

    elif action == "refresh":
        name = params.get("name")
        if not name:
            return {"success": False, "error": "name required for refresh"}
        return await action_refresh(
            name=name,
            project_path=project_path,
        )

    elif action == "remove":
        name = params.get("name")
        if not name:
            return {"success": False, "error": "name required for remove"}
        return await action_remove(
            name=name,
            project_path=project_path,
        )

    return {"success": False, "error": f"Unknown action: {action}"}


# CLI entry point for subprocess execution
if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser(description="MCP Manager")
    parser.add_argument("--params", required=True, help="Parameters as JSON")
    parser.add_argument("--project-path", required=True, help="Project path")
    parser.add_argument("--debug", action="store_true", help="Enable debug logging")

    args = parser.parse_args()

    if args.debug:
        logging.basicConfig(level=logging.DEBUG)
    else:
        logging.basicConfig(level=logging.INFO)

    try:
        params = json.loads(args.params)
        action = params.pop("action", None)
        if not action:
            print(json.dumps({"success": False, "error": "action required in params"}))
            sys.exit(1)
    except json.JSONDecodeError as e:
        print(json.dumps({"success": False, "error": f"Invalid params JSON: {e}"}))
        sys.exit(1)

    try:
        result = asyncio.run(execute_action(action, Path(args.project_path), params))
        print(json.dumps(result, indent=2), flush=True)
        sys.exit(0 if result.get("success") else 1)
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}), flush=True)
        sys.exit(1)
