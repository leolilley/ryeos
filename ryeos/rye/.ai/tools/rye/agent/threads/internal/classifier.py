# rye:signed:2026-02-23T08:17:58Z:495534590b2385b41cb7eaafc6402178290e208670ca07fd8a10eedef9476cea:89ZxeY-JjfYNRs-HEdiy-6h6yHHoixZN-sfCj1iIJzDts5j7fBgT5cxe3yRkQ6EZACCA3T6y5ToVz_vWi9AkDw==:9fbfabe975fa5a7f
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
