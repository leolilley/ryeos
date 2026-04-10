# rye:signed:2026-04-10T00:57:19Z:c5c2c024b88aa6f7eb92a64d4dee91e4ee07456d5cb63683eed2d7d0e0cad101:SdHacKHFtI7Jvjny3YRNUV8-LO7MsbDPq40WtLP0gj2xAMDDxKCeLl8cv0gyySivcJqFRtycwXhiB9-2K4b6BA:4b987fd4e40303ac
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Check cancellation requests"

from typing import Dict


def execute(params: Dict, project_path: str) -> Dict:
    """Check if thread cancellation has been requested."""
    from pathlib import Path
    import importlib.util

    state_path = Path(__file__).parent.parent / "persistence" / "state_store.py"
    spec = importlib.util.spec_from_file_location("state_store", state_path)
    state_store_mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(state_store_mod)

    thread_id = params.get("thread_id")

    if not thread_id:
        return {"success": False, "error": "Missing thread_id in context"}

    store = state_store_mod.StateStore(Path(project_path), thread_id)
    cancelled = store.is_cancel_requested()

    return {"success": True, "cancelled": cancelled}
