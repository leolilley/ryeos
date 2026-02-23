# rye:signed:2026-02-23T00:42:51Z:42ab326ce86e284bead7e9122126b053d03dd43e1e83c2b39dc7109ec50c4a3a:uWimeriMQETCmt8Aqz_SW0e5UCqdbZnP_u3_Uq76AFQh85QWcBQ77DwcWpw-kOhQUiFrxa89ewJrOY9QXULmBw==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Persist harness state"

from typing import Any, Dict


def execute(params: Dict, project_path: str) -> Dict:
    """Persist thread harness state."""
    from pathlib import Path
    import importlib.util

    state_path = Path(__file__).parent.parent / "persistence" / "state_store.py"
    spec = importlib.util.spec_from_file_location("state_store", state_path)
    state_store_mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(state_store_mod)

    ctx = params.get("_thread_context", {})
    thread_id = ctx.get("thread_id")
    state = params.get("state", {})

    if not thread_id:
        return {"success": False, "error": "Missing thread_id in context"}

    store = state_store_mod.StateStore(Path(project_path), thread_id)
    store.save(state)

    return {"success": True, "persisted": True}
