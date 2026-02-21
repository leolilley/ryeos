# rye:signed:2026-02-21T05:56:40Z:954a58e4b55dfccf88668a9fae76af4f16db64ed338189ccc658730d959fd764:C5R5DMIVQQkK81CoSZD8DUGjmRzIU0Hzjxw3oZaUI9ovwJkomBbK2F9PGee4prDm-qgmfCXC8D3Z0-fFJ69pAw==:9fbfabe975fa5a7f
__version__ = "1.1.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
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
