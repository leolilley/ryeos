# rye:signed:2026-02-22T09:00:56Z:bc6324579a74565055d20715d4aa5e760986c9c87117530135494405980a6814:AXLnCCB1ZAmD5mDaKeRkBK5WOaVj-4JBy3VX3flzy43ggee1V63pNUXT2cZ8jZLF4E5MLvRHFjChxP0Xgc_vBQ==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
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

    ctx = params.get("_thread_context", {})
    thread_id = ctx.get("thread_id")

    if not thread_id:
        return {"success": False, "error": "Missing thread_id in context"}

    store = state_store_mod.StateStore(Path(project_path), thread_id)
    cancelled = store.is_cancel_requested()

    return {"success": True, "cancelled": cancelled}
