# rye:signed:2026-04-19T09:49:53Z:ed1be97a724eab8eceb3ff5a90515a169070ece81bea65b596b178b8c9f03c0b:qyZEZzkpmbwgnaJJY2EI3lHu1jzHyrOM+aALgE8pXJkWYT7cxTx8YlJj6mYa7NcW+00QVvXI3GcOECHGfJkkBg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Check thread limits"

from typing import Dict

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "limit_type": {"type": "string"},
        "current_value": {"type": "number"},
        "max_value": {"type": "number"},
    },
    "required": ["limit_type", "current_value", "max_value"],
}


def execute(params: Dict, project_path: str) -> Dict:
    """Check if a limit is exceeded."""
    from pathlib import Path
    import importlib.util

    loader_path = Path(__file__).parent.parent / "loaders" / "resilience_loader.py"
    spec = importlib.util.spec_from_file_location("resilience_loader", loader_path)
    resilience_loader = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(resilience_loader)

    config = resilience_loader.load(Path(project_path))

    limit_type = params.get("limit_type")
    current = params.get("current_value")
    maximum = params.get("max_value")

    if current >= maximum:
        on_exceed = (
            config.get("limits", {}).get("enforcement", {}).get("on_exceed", "fail")
        )
        return {
            "success": True,
            "exceeded": True,
            "limit_type": limit_type,
            "current": current,
            "max": maximum,
            "action": on_exceed,
        }

    return {"success": True, "exceeded": False}
