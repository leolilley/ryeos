# rye:signed:2026-02-25T00:02:14Z:5eb308f51fed03e6106023bc5366a3d6fe328576ab2f16a5ba98d23d98d5de2a:7-7aMXx14ZZZEAkSS3pIxqMMyYzqXTmCPN3q8gNOoxV5oq1rYzmLHFxcS94_OBkOYMZtjFThGuwy0qc0Bq15Dg==:9fbfabe975fa5a7f
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

    ctx = params.get("_thread_context", {})
    thread_id = ctx.get("thread_id")

    if not thread_id:
        return {"success": False, "error": "Missing thread_id in context"}

    store = state_store_mod.StateStore(Path(project_path), thread_id)
    cancelled = store.is_cancel_requested()

    return {"success": True, "cancelled": cancelled}
