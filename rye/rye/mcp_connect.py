"""MCP Connect module entry point.

Thin wrapper that resolves server paths and delegates to connect logic.
Called via: python -m rye.mcp_connect --server ID --tool NAME --params JSON --project-path PATH

This keeps the executor data-driven - no special MCP logic in PrimitiveExecutor.
Path resolution happens here using standard rye utilities.
"""

import argparse
import asyncio
import json
import sys
from pathlib import Path
from typing import Any, Dict, Optional


def resolve_server_config(server_id: str, project_path: Path) -> Optional[Path]:
    """Resolve server config path from tool ID.
    
    Searches: project > user > system spaces.
    
    Args:
        server_id: Relative tool ID like "mcp/servers/context7"
        project_path: Project root path
        
    Returns:
        Absolute path to server config YAML, or None if not found
    """
    from rye.utils.path_utils import get_user_space, get_system_space
    
    extensions = [".yaml", ".yml"]
    
    search_order = [
        project_path / ".ai" / "tools",
        get_user_space() / "tools",
        get_system_space() / "tools",
    ]
    
    for base_path in search_order:
        if not base_path.exists():
            continue
        for ext in extensions:
            config_path = base_path / f"{server_id}{ext}"
            if config_path.is_file():
                return config_path
    
    return None


async def execute_mcp_tool(
    server_id: str,
    tool_name: str,
    params: Dict[str, Any],
    project_path: Path,
) -> Dict[str, Any]:
    """Execute MCP tool call.
    
    Resolves server config path, then delegates to connect logic.
    """
    # Resolve server config
    server_config = resolve_server_config(server_id, project_path)
    if not server_config:
        return {
            "success": False,
            "error": f"Server config not found: {server_id}",
            "searched": ["project/.ai/tools", "~/.ai/tools", "system/tools"],
        }
    
    # Import connect logic from .ai/tools
    # We import dynamically to use the data tool, not embed logic here
    tools_path = Path(__file__).parent / ".ai" / "tools"
    
    import importlib.util
    connect_path = tools_path / "rye" / "core" / "mcp" / "connect.py"
    
    if not connect_path.exists():
        return {
            "success": False,
            "error": f"connect.py not found at {connect_path}",
        }
    
    spec = importlib.util.spec_from_file_location("connect", connect_path)
    if not spec or not spec.loader:
        return {"success": False, "error": "Failed to load connect module"}
    
    connect = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(connect)
    
    # Call connect's execute function
    return await connect.execute_with_server_config(
        server_config_path=str(server_config),
        tool_name=tool_name,
        params=params,
        project_path=str(project_path),
    )


def main():
    parser = argparse.ArgumentParser(description="MCP Connect")
    parser.add_argument("--server", required=True, help="Server tool ID (e.g., mcp/servers/context7)")
    parser.add_argument("--tool", required=True, help="MCP tool name to call")
    parser.add_argument("--params", default="{}", help="Tool parameters as JSON")
    parser.add_argument("--project-path", required=True, help="Project root path")
    
    args = parser.parse_args()
    
    try:
        params = json.loads(args.params)
    except json.JSONDecodeError as e:
        print(json.dumps({"success": False, "error": f"Invalid params JSON: {e}"}))
        sys.exit(1)
    
    try:
        result = asyncio.run(execute_mcp_tool(
            server_id=args.server,
            tool_name=args.tool,
            params=params,
            project_path=Path(args.project_path),
        ))
        print(json.dumps(result, indent=2), flush=True)
        sys.exit(0 if result.get("success") else 1)
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}), flush=True)
        sys.exit(1)


if __name__ == "__main__":
    main()
