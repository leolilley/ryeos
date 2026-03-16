# rye:signed:2026-03-16T09:53:44Z:f5622fcbe33fbeba8bf66868e20a1f0e97f2d5033a72536d20ea58646444b5e4:vqdUBqB-E_2V5EeWKbHwzA5bI782LHns0ecqkAEXTgoPrxWkW2BQiaqwRUPnNTpxlrWCU9fFG-XlgYl_Lc3MAA==:4b987fd4e40303ac
"""Send an email via the configured email provider."""

import argparse
import asyncio
import json
import sys
import yaml
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/email"
__tool_description__ = "Send an email — resolve provider and execute send action"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "to": {"type": "string", "description": "Recipient email address"},
        "subject": {"type": "string", "description": "Email subject line"},
        "body": {"type": "string", "description": "Email body text"},
        "from": {"type": "string", "description": "Sending inbox address"},
        "from_name": {"type": "string", "description": "Sender display name"},
    },
    "required": ["to", "subject", "body"],
}

CONFIG_RESOLVE = {
    "path": "email/email.yaml",
    "mode": "deep_merge",
}


async def execute(params: dict, project_path: str) -> dict:
    """Send an email using the configured provider's action mapping."""
    from rye.tools.execute import ExecuteTool

    config = params.get("resolved_config", {})
    provider_name = config.get("provider", {}).get("default")
    if not provider_name:
        return {"success": False, "error": "No email provider configured — set provider.default in .ai/config/email/email.yaml"}

    agent_config = config.get("agent", {})
    from_address = params.get("from") or agent_config.get("inbox")
    from_name = params.get("from_name") or agent_config.get("name")
    if not from_address:
        return {"success": False, "error": "No sending address — set agent.inbox in email config or pass 'from' parameter"}

    # Load provider YAML
    provider = _load_provider(project_path, provider_name)
    if not provider:
        return {"success": False, "error": f"Provider '{provider_name}' not found"}

    mcp_server = provider.get("mcp_server")
    send_action = provider.get("actions", {}).get("send")
    if not send_action:
        return {"success": False, "error": f"Provider '{provider_name}' has no 'send' action"}

    executor = ExecuteTool(project_path=project_path)

    # Build the canonical params
    send_params = {
        "to": params["to"],
        "subject": params["subject"],
        "body": params["body"],
        "from": from_address,
        "from_name": from_name or "",
    }

    # Multi-step send (e.g., CK: create → approve → schedule)
    if "steps" in send_action:
        prev_result = {}
        for step in send_action["steps"]:
            tool_name = step["tool"]
            step_params = _resolve_params(step.get("params_map", {}), send_params, prev_result)

            mcp_tool_id = f"mcp/{mcp_server}/{tool_name.replace('.', '/')}"
            result = await executor.handle(
                item_type="tool",
                item_id=mcp_tool_id,
                project_path=project_path,
                parameters=step_params,
            )

            if result.get("status") == "error":
                return {"success": False, "error": f"Step '{tool_name}' failed: {result.get('error')}", "step": tool_name}

            # Extract data from result envelope
            prev_result = result.get("data", result)

        email_id = prev_result.get("email_id") or prev_result.get("id")
        return {
            "success": True,
            "email_id": email_id,
            "status": "sent",
            "message_id": prev_result.get("message_id"),
        }

    # Single-step send (e.g., Gmail)
    tool_name = send_action.get("tool")
    step_params = _resolve_params(send_action.get("params_map", {}), send_params, {})

    mcp_tool_id = f"mcp/{mcp_server}/{tool_name.replace('.', '/')}"
    result = await executor.handle(
        item_type="tool",
        item_id=mcp_tool_id,
        project_path=project_path,
        parameters=step_params,
    )

    if result.get("status") == "error":
        return {"success": False, "error": f"Send failed: {result.get('error')}"}

    data = result.get("data", result)
    return {
        "success": True,
        "email_id": data.get("email_id") or data.get("id"),
        "status": "sent",
        "message_id": data.get("message_id"),
    }


def _load_provider(project_path: str, provider_name: str) -> dict:
    """Load a provider YAML from the tools directory."""
    # Check project space first, then system bundle
    for base in [Path(project_path), *_system_paths()]:
        provider_path = base / ".ai" / "tools" / "rye" / "email" / "providers" / provider_name / f"{provider_name}.yaml"
        if provider_path.exists():
            with open(provider_path) as f:
                return yaml.safe_load(f)
    return {}


def _system_paths():
    """Find system bundle paths for provider resolution."""
    import importlib.resources
    try:
        # ryeos_email bundle
        ref = importlib.resources.files("ryeos_email")
        return [Path(str(ref))]
    except Exception:
        return []


def _resolve_params(params_map: dict, send_params: dict, prev_result: dict) -> dict:
    """Resolve params_map values against send_params and previous results."""
    resolved = {}
    for target_key, source_expr in params_map.items():
        if isinstance(source_expr, str) and source_expr.startswith("$prev."):
            # Reference to previous step's result
            field = source_expr[6:]  # strip "$prev."
            resolved[target_key] = prev_result.get(field)
        elif isinstance(source_expr, str) and source_expr in send_params:
            resolved[target_key] = send_params[source_expr]
        else:
            resolved[target_key] = source_expr
    return resolved


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    p = json.loads(sys.stdin.read())
    result = asyncio.run(execute(p, args.project_path))
    print(json.dumps(result, default=str))
