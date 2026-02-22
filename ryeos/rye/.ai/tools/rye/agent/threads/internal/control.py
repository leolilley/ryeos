# rye:signed:2026-02-22T09:00:56Z:123c9ba7d82a38e552738e2dbda57f6a3df3ed0f6909105934fc27a281af1ab7:BiWPlfN41wgYJx2lC6yJVe_Uu6dcjFoTnBE9QrXBdErdPhzh4ziyi5wCjbih9hnCgWqgHapgz-PKWUcC-_RuDA==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Handle thread control actions"

from typing import Dict, Optional

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": [
                "retry",
                "fail",
                "abort",
                "continue",
                "escalate",
                "suspend",
                "skip",
            ],
        },
        "error": {"type": "string"},
        "limit_type": {"type": "string"},
        "current_value": {"type": "number"},
        "suspend_reason": {"type": "string"},
    },
    "required": ["action"],
}


def execute(params: Dict, project_path: str) -> Optional[Dict]:
    """Execute a control action.

    Returns None for continue/skip, or a result dict for terminating actions.
    The runner interprets the return value to determine flow control.
    """
    action = params.get("action", "continue")

    if action in ("continue", "skip"):
        return None

    if action == "retry":
        return {"action": "retry"}

    if action == "fail":
        return {
            "success": False,
            "error": params.get("error", "Hook triggered failure"),
        }

    if action == "abort":
        return {"success": False, "aborted": True, "error": "Aborted by hook"}

    if action == "suspend":
        return {
            "success": False,
            "suspended": True,
            "error": params.get("suspend_reason", "Suspended by hook"),
        }

    if action == "escalate":
        return {
            "success": False,
            "suspended": True,
            "escalated": True,
            "error": "Escalation requested",
            "escalation": {
                "limit_type": params.get("limit_type"),
                "current_value": params.get("current_value"),
            },
        }

    return None
