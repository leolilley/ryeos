# rye:signed:2026-04-20T05:46:18Z:fa6cfe845e4abf1776e7c7f9c994b02a7c0d998f740d6307e4576bcab7f669c8:rKyg42b3syDG2SbjDjXIlIly9Ro-TRU2Vw4NQA6aXwBW0sBuQaRnw1OTbarvgaMEAgGu55kCuWqQoZuZ3HkaAw:4b987fd4e40303ac
__version__ = "1.2.0"
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
    """Budget operations are daemon-owned in v3."""
    return {
        "success": False,
        "error": (
            "Budget operations are daemon-owned in v3; "
            "the Python budget ledger is no longer authoritative. "
            "Use daemon RPC for budget queries."
        ),
    }
