# rye:validated:2026-02-04T23:40:35Z:258374a5a7dfda73569629d5e0d8e577012fab3ba2783e326271b3848a6a4d2e
"""MCP HTTP Runtime - Execute MCP tools via HTTP transport.

Layer 2 runtime with __executor_id__ = "rye/core/primitives/subprocess".
Runs connect.py script which handles MCP SDK connections.

Use this runtime for MCP tools discovered from HTTP servers.
"""

__version__ = "1.0.0"
__tool_type__ = "runtime"
__executor_id__ = "rye/core/primitives/subprocess"
__category__ = "rye/core/runtimes"
__tool_description__ = "MCP HTTP runtime - executes MCP tools via HTTP transport"

ENV_CONFIG = {
    "interpreter": {
        "type": "venv_python",
        "var": "RYE_PYTHON",
        "fallback": "python3",
    },
    "env": {
        "PYTHONUNBUFFERED": "1",
    },
}

# server_config_path is stored as a template in the tool YAML at discovery time
# e.g., "{project_path}/.ai/tools/mcp/servers/context7.yaml"
CONFIG = {
    "command": "${RYE_PYTHON}",
    "args": [
        "{system_space}/tools/rye/core/mcp/connect.py",
        "--server-config", "{server_config_path}",
        "--tool", "{tool_name}",
        "--params", "{params_json}",
        "--project-path", "{project_path}",
    ],
    "timeout": 60,
}

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "server": {
            "type": "string",
            "description": "Relative tool ID to server config (e.g., mcp/servers/context7)",
        },
        "tool_name": {
            "type": "string",
            "description": "MCP tool name to call",
        },
        "timeout": {
            "type": "number",
            "description": "Execution timeout in seconds",
            "default": 60,
        },
    },
    "required": ["server", "tool_name"],
}
