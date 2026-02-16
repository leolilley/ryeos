# rye:signed:2026-02-16T05:32:26Z:e1d850d0740d73bebbd866fdb73126e76185afa942259c0d6f66d28e4aff7969:w7KWKAyVVCeSSPLHJTFDqK_gzaG0UsAB2HCrg6_yTJIbVswZFh40VQJEFAh9L5oBo81ZHNJHAO4c04C2dfGJBA==:440443d0858f0199
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Track LLM costs"

from typing import Any, Dict


def execute(params: Dict, project_path: str) -> Dict:
    """Track and report LLM costs."""
    ctx = params.get("_thread_context", {})
    cost = ctx.get("cost", {})

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
