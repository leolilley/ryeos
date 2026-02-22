# rye:signed:2026-02-22T09:00:56Z:ade43de314f8a1205ae0ceb8e5ecfa4fe355e93342c69676467012e357ea2691:HxJovyLIcBkGi2yug1QnKTJH5rQi3aLmX9xU-2BzrLXF_isvYp25VfsMMwGTB-B4sAhU26T78rbrTXRMynYNAg==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
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
