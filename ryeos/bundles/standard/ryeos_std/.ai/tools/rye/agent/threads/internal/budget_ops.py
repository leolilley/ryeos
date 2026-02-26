# rye:signed:2026-02-26T05:52:24Z:cc2b2b0cc22da1502c2fe0e84b597c8f76ca4699c92fd9e4c79322d40df6161b:v7dXMfg-cHld5z3wTH6XgWuh1P21GkMbO8R6a2Ui6IIY0A7V8AzaObxxNCGPN8eaxN2KWlKYH3LFfszDSyBoAA==:4b987fd4e40303ac
__version__ = "1.1.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Budget operations"

from typing import Dict

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {
            "type": "string",
            "enum": [
                "reserve", "report_actual", "release", "check_remaining",
                "can_spawn", "increment_actual", "get_tree_spend",
            ],
        },
        "thread_id": {"type": "string"},
        "parent_thread_id": {"type": "string"},
        "amount": {"type": "number"},
        "final_status": {"type": "string"},
    },
    "required": ["operation", "thread_id"],
}


def execute(params: Dict, project_path: str) -> Dict:
    """Execute budget operation."""
    from pathlib import Path

    from module_loader import load_module
    _anchor = Path(__file__).parent.parent
    budgets = load_module("persistence/budgets", anchor=_anchor)

    operation = params["operation"]
    thread_id = params["thread_id"]
    ledger = budgets.get_ledger(Path(project_path))

    if operation == "reserve":
        parent_id = params.get("parent_thread_id")
        amount = params.get("amount", 0.0)
        ledger.reserve(thread_id, amount, parent_id)
        return {"success": True, "reserved": amount}

    if operation == "report_actual":
        amount = params.get("amount", 0.0)
        ledger.report_actual(thread_id, amount)
        return {"success": True, "reported": amount}

    if operation == "release":
        ledger.release(thread_id, params.get("final_status", "completed"))
        return {"success": True, "released": True}

    if operation == "check_remaining":
        remaining = ledger.get_remaining(thread_id)
        return {"success": True, "remaining": remaining}

    if operation == "can_spawn":
        return ledger.can_spawn(thread_id, params.get("amount", 0.0))

    if operation == "increment_actual":
        ledger.increment_actual(thread_id, params.get("amount", 0.0))
        return {"success": True}

    if operation == "get_tree_spend":
        return {"success": True, **ledger.get_tree_spend(thread_id)}

    return {"success": False, "error": f"Unknown operation: {operation}"}
