# rye:signed:2026-04-19T09:49:53Z:495534590b2385b41cb7eaafc6402178290e208670ca07fd8a10eedef9476cea:K2cMshdc162WxWQ1Oinwz8ZilG6C4fSnHcqLDVdlmsDbeRGqth3qLO6ycjBCDuNUX280g+bgUkrrHD/o9lRpCg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Classify errors using config patterns"

from pathlib import Path
from typing import Dict


def execute(params: Dict, project_path: str) -> Dict:
    """Classify an error using error_classification.yaml patterns."""
    import importlib.util

    loader_path = Path(__file__).parent.parent / "loaders" / "error_loader.py"
    spec = importlib.util.spec_from_file_location("error_loader", loader_path)
    error_loader = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(error_loader)

    return error_loader.classify(
        Path(project_path),
        {
            "error": params.get("error", {}),
            "status_code": params.get("status_code"),
            "headers": params.get("headers", {}),
        },
    )
