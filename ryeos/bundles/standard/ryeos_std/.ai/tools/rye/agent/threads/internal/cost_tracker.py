# rye:signed:2026-04-10T00:57:19Z:3bdcab8d0463276c22ba6378fcd89ff61d668b681893aa668cdd1de7dba66c13:P1cjJ52X4vObvOVZ8EFgFtQGTa1BVNXunTmVm2Ew3tVjt4VCeMnk8NtiLozyUMA-0uRFWuJ9pqDFxw1e5YYQBA:4b987fd4e40303ac
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Track LLM costs"

from typing import Any, Dict


def execute(params: Dict, project_path: str) -> Dict:
    """Track and report LLM costs."""
    cost = params.get("cost", {})

    operation = params.get("operation", "report")

    if operation == "report":
        return {
            "success": True,
            "cost": {
                "turns": cost.get("turns", 0),
                "input_tokens": cost.get("input_tokens", 0),
                "output_tokens": cost.get("output_tokens", 0),
                "spend": cost.get("spend", 0.0),
            },
        }

    if operation == "add":
        cost["turns"] = cost.get("turns", 0) + params.get("turns", 0)
        cost["input_tokens"] = cost.get("input_tokens", 0) + params.get(
            "input_tokens", 0
        )
        cost["output_tokens"] = cost.get("output_tokens", 0) + params.get(
            "output_tokens", 0
        )
        cost["spend"] = cost.get("spend", 0.0) + params.get("spend", 0.0)
        return {"success": True, "cost": cost}

    return {"success": False, "error": f"Unknown operation: {operation}"}
