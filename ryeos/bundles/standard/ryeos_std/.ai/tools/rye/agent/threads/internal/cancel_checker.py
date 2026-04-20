# rye:signed:2026-04-19T09:49:53Z:3656240b22cc1de89be4e94ed0a5a6294e122a7614f9ee21157d15ff6c2a0a37:Few5K10W9Bcb+SLonRorylT32Q13XiU1IzpgqTOhXLFAvbPsjCVmjmJbeOI7Pzz1p362wer5za/WSLnMw4pWBg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
__version__ = "1.1.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Check cancellation requests"

from typing import Dict


def execute(params: Dict, project_path: str) -> Dict:
    """Check if thread cancellation has been requested via daemon."""
    from rye.runtime.daemon_rpc import ThreadLifecycleClient, resolve_daemon_socket_path, RpcError

    thread_id = params.get("thread_id")
    if not thread_id:
        return {"success": False, "error": "Missing thread_id"}

    socket_path = resolve_daemon_socket_path()
    if not socket_path:
        return {"success": True, "cancelled": False}

    try:
        client = ThreadLifecycleClient(socket_path)
        record = client.get_thread(thread_id)
        if not record:
            return {"success": True, "cancelled": False}
        thread = record.get("thread") or {}
        status = thread.get("status", "")
        cancelled = status in ("cancelled", "killed")
        return {"success": True, "cancelled": cancelled}
    except (OSError, RuntimeError, RpcError):
        return {"success": True, "cancelled": False}
