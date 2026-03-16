# rye:signed:2026-03-16T11:23:58Z:095138272452334ef075e14a726d54d904fb0808eeeaeea80c00b244fec42072:TV2mZPRkw3UzL6GFHCBnn4dwzYEPQbUzLQW0x9Gn2rw85-yqFIviqiJMkQVJ34vwk6bt1cELECt-_rmla4XtCg==:4b987fd4e40303ac
"""Forward an email to a private address with agent context."""

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
__tool_description__ = "Forward an email with agent notes to the configured forward address"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "email_id": {"type": "string", "description": "Original email ID to forward"},
        "forward_to": {"type": "string", "description": "Override forward address (uses config default if omitted)"},
        "classification": {"type": "string", "description": "Email classification from router"},
        "lead_context": {"type": "string", "description": "Lead data summary"},
        "suggested_response": {"type": "string", "description": "Agent-drafted suggested reply"},
    },
    "required": ["email_id", "classification"],
}

CONFIG_RESOLVE = {
    "path": "email/email.yaml",
    "mode": "deep_merge",
}


async def execute(params: dict, project_path: str) -> dict:
    """Forward an email with agent context prepended."""
    from rye.tools.execute import ExecuteTool

    config = params.get("resolved_config", {})
    provider_name = config.get("provider", {}).get("default")
    if not provider_name:
        return {"success": False, "error": "No email provider configured — set provider.default in .ai/config/email/email.yaml"}

    agent_config = config.get("agent", {})
    forward_to = params.get("forward_to") or agent_config.get("forward_to")
    if not forward_to:
        return {"success": False, "error": "No forward address — set agent.forward_to in email config or pass 'forward_to' parameter"}

    agent_inbox = agent_config.get("inbox")
    if not agent_inbox:
        return {"success": False, "error": "No agent inbox — set agent.inbox in email config"}

    # Load provider
    provider = _load_provider(project_path, provider_name)
    if not provider:
        return {"success": False, "error": f"Provider '{provider_name}' not found"}

    mcp_server = provider.get("mcp_server")
    executor = ExecuteTool(project_path=project_path)

    # Fetch original email
    get_action = provider.get("actions", {}).get("get")
    if not get_action:
        return {"success": False, "error": f"Provider '{provider_name}' has no 'get' action"}

    get_tool = get_action.get("tool")
    get_params = _resolve_params(get_action.get("params_map", {}), {"email_id": params["email_id"]}, {})

    mcp_tool_id = f"mcp/{mcp_server}/{get_tool.replace('.', '/')}"
    fetch_result = await executor.handle(
        item_type="tool",
        item_id=mcp_tool_id,
        project_path=project_path,
        parameters=get_params,
    )

    if fetch_result.get("status") == "error":
        return {"success": False, "error": f"Failed to fetch email: {fetch_result.get('error')}"}

    original = fetch_result.get("data", fetch_result)
    original_from = original.get("from_address") or original.get("from") or "unknown"
    original_subject = original.get("subject", "(no subject)")
    original_body = original.get("body") or original.get("body_text") or ""

    # Build forward body
    classification = params.get("classification", "unclassified")
    lead_context = params.get("lead_context", "N/A")
    suggested_response = params.get("suggested_response", "")

    forward_body = f"""=== AGENT NOTES ===
Classification: {classification}
Lead: {lead_context}
Reply-via: Reply to this email — your response will be routed through the agent and sent from the correct domain.

"""
    if suggested_response:
        forward_body += f"""=== SUGGESTED RESPONSE ===
{suggested_response}

"""
    forward_body += f"""=== ORIGINAL EMAIL ===
From: {original_from}
Subject: {original_subject}

{original_body}"""

    forward_subject = f"[Agent] {classification}: {original_subject}"

    # Send the forward using the send tool
    # Import the send tool's execute function directly since we're in the same package
    send_module_path = Path(__file__).parent / "send.py"
    if send_module_path.exists():
        import importlib.util
        spec = importlib.util.spec_from_file_location("send_tool", send_module_path)
        send_mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(send_mod)
        send_result = await send_mod.execute(
            {
                "to": forward_to,
                "subject": forward_subject,
                "body": forward_body,
                "from": agent_inbox,
                "resolved_config": config,
            },
            project_path,
        )
    else:
        # Fallback: use ExecuteTool to call rye/email/send
        send_result = await executor.handle(
            item_type="tool",
            item_id="rye/email/send",
            project_path=project_path,
            parameters={
                "to": forward_to,
                "subject": forward_subject,
                "body": forward_body,
                "from": agent_inbox,
            },
        )

    if not send_result.get("success", False) and send_result.get("status") == "error":
        return {"success": False, "error": f"Failed to send forward: {send_result.get('error')}"}

    data = send_result.get("data", send_result)
    return {
        "success": True,
        "forwarded_email_id": data.get("email_id") or data.get("id"),
    }


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


def _resolve_params(params_map: dict, source_params: dict, prev_result: dict) -> dict:
    """Resolve params_map values against source params and previous results."""
    resolved = {}
    for target_key, source_expr in params_map.items():
        if isinstance(source_expr, str) and source_expr.startswith("$prev."):
            field = source_expr[6:]
            resolved[target_key] = prev_result.get(field)
        elif isinstance(source_expr, str) and source_expr in source_params:
            resolved[target_key] = source_params[source_expr]
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
