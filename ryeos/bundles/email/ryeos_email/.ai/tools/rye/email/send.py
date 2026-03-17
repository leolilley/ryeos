# rye:signed:2026-03-17T02:33:40Z:301bafc9f9b248732868f64d0453f23ba4eb641862ba9477aca3b538b7296802:uLN4xAzkeV2L6RgFSZrcJCIF7JrdyK0p_O_XsesZ1WVnkGxY3mNwRDEGnnWdqKSf8GYz0Mzv8K3me1I0USQcBQ==:4b987fd4e40303ac
"""Send an email via the configured email provider."""

import argparse
import asyncio
import json
import sys
import yaml
from pathlib import Path

__version__ = "1.1.0"
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

    # Multi-step send (e.g., CK: create → approve → schedule)
    if "steps" in send_action:
        return await _multi_step_send(
            mcp_server, send_action["steps"], params, from_address, from_name, project_path,
        )

    # Single-step send (e.g., Gmail)
    step = send_action
    return await _single_step_send(
        mcp_server, step, params, from_address, from_name, project_path,
    )


async def _multi_step_send(
    mcp_server: str, steps: list, params: dict,
    from_address: str, from_name: str, project_path: str,
) -> dict:
    """Execute a multi-step send pipeline (create → approve → schedule)."""
    email_id = None

    for step in steps:
        action = step["action"]
        type_name = step["type"]

        step_params = _build_step_params(
            action, type_name, params, from_address, from_name, email_id,
        )

        result = await _execute_mcp(mcp_server, type_name, action, step_params, project_path)
        if not result.get("success", False):
            error = result.get("error") or result.get("message", "unknown error")
            return {"success": False, "error": f"Step '{type_name}.{action}' failed: {error}", "step": f"{type_name}.{action}"}

        # Track email_id through the pipeline
        email_id = result.get("email_id") or email_id

    return {
        "success": True,
        "email_id": email_id,
        "status": "sent",
    }


async def _single_step_send(
    mcp_server: str, step: dict, params: dict,
    from_address: str, from_name: str, project_path: str,
) -> dict:
    """Execute a single-step send (e.g., Gmail)."""
    action = step["action"]
    type_name = step["type"]

    step_params = _build_step_params(
        action, type_name, params, from_address, from_name, None,
    )

    result = await _execute_mcp(mcp_server, type_name, action, step_params, project_path)
    if not result.get("success", False):
        error = result.get("error") or result.get("message", "unknown error")
        return {"success": False, "error": f"Send failed: {error}"}

    return {
        "success": True,
        "email_id": result.get("email_id") or result.get("id"),
        "status": "sent",
    }


def _build_step_params(
    action: str, type_name: str, params: dict,
    from_address: str, from_name: str, email_id: str | None,
) -> dict:
    """Build MCP params for a pipeline step.

    Maps canonical send params to provider-specific field names.
    Each provider type has its own param conventions.
    """
    if type_name == "primary_email" and action == "create":
        return {
            "to_emails": [params["to"]],
            "from_email": from_address,
            "from_name": from_name or "",
            "subject": params["subject"],
            "body_text": params["body"],
        }
    elif type_name == "primary_email" and action == "approve":
        return {"entity_id": email_id}
    elif type_name == "scheduler" and action == "schedule":
        return {
            "email_ids": [email_id],
            "email_type": "primary",
            "scheduled_time": "immediate",
            "dry_run": False,
        }
    # Gmail-style single send
    elif action == "send":
        return {
            "to": params["to"],
            "from": from_address,
            "subject": params["subject"],
            "body": params["body"],
        }
    else:
        return {}


async def _execute_mcp(
    mcp_server: str, type_name: str, action: str,
    step_params: dict, project_path: str,
) -> dict:
    """Execute an MCP action via the campaign-kiwi MCP server."""
    from rye.tools.execute import ExecuteTool

    executor = ExecuteTool(project_path=project_path)
    mcp_tool_id = f"mcp/{mcp_server}/{type_name}/{action}"

    result = await executor.handle(
        item_type="tool",
        item_id=mcp_tool_id,
        project_path=project_path,
        parameters=step_params,
    )

    if result.get("status") == "error":
        return {"success": False, "error": result.get("error", "MCP call failed")}

    data = result.get("data", result)
    if isinstance(data, dict):
        data.setdefault("success", True)
    return data


def _load_provider(project_path: str, provider_name: str) -> dict:
    """Load a provider YAML from the tools directory."""
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
        ref = importlib.resources.files("ryeos_email")
        return [Path(str(ref))]
    except Exception:
        return []


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    p = json.loads(sys.stdin.read())
    result = asyncio.run(execute(p, args.project_path))
    print(json.dumps(result, default=str))
