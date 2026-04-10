# rye:signed:2026-04-10T00:57:19Z:396cb19c90495d313d3be84b7e933a589d0cf541bacb77b8922d74df1d0ca665:M1tZ5JRxZwVB2-ENsOJOzm73tltrprJAdIfwQJXaZ5iQM1CDl86x5-ZdrtQzNDAan4mrRUOENcgbqdjZbQraDA:4b987fd4e40303ac
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

    thread_id = params.get("thread_id")
    state = params.get("state", {})

    if not thread_id:
        return {"success": False, "error": "Missing thread_id in context"}

    store = state_store_mod.StateStore(Path(project_path), thread_id)
    store.save(state)

    return {"success": True, "persisted": True}
