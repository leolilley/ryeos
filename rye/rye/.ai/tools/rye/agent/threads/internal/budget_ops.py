# rye:signed:2026-02-16T05:32:26Z:a221257c5837b129f0e60127a84cb199daf9a332bb5ce0fcc5f2e4d12945411f:10RcFMdXpKsqtX1s5PBcXUNwuc3GDMJMsh_UqxMWFx9x4YjhBu1imjJPxBBI23oAmH7r5OxpmFk4lV-jatrzCw==:440443d0858f0199
__version__ = "1.0.0"
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
            "enum": ["reserve", "report_actual", "release", "check_remaining"],
        },
        "thread_id": {"type": "string"},
        "parent_thread_id": {"type": "string"},
        "amount": {"type": "number"},
    },
    "required": ["operation", "thread_id"],
}


def execute(params: Dict, project_path: str) -> Dict:
    """Execute budget operation."""
    from pathlib import Path
    import importlib.util

    budgets_path = Path(__file__).parent.parent / "persistence" / "budgets.py"
    spec = importlib.util.spec_from_file_location("budgets", budgets_path)
    budgets = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(budgets)

    operation = params.get("operation")
    thread_id = params.get("thread_id")
    ledger = budgets.get_ledger(Path(project_path))

    if operation == "reserve":
        parent_id = params.get("parent_thread_id")
        amount = params.get("amount", 0.0)
        success = ledger.reserve(thread_id, amount, parent_thread_id=parent_id)
        return {"success": success, "reserved": amount if success else 0}

    if operation == "report_actual":
        amount = params.get("amount", 0.0)
        ledger.report_actual(thread_id, amount)
        return {"success": True, "reported": amount}

    if operation == "release":
        ledger.release(thread_id)
        return {"success": True, "released": True}

    if operation == "check_remaining":
        remaining = ledger.get_remaining(thread_id)
        return {"success": True, "remaining": remaining}

    return {"success": False, "error": f"Unknown operation: {operation}"}
