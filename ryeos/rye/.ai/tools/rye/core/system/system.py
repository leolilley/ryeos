# rye:signed:2026-02-25T00:02:14Z:71927107d2ccec34ddfdd51384196366113b2105d4220db0039e80df851bb84b:IechhYscRsdxtXW5ALT3L5JhlRB_0njoN9SovLPzlDToqECmziN4euEnBdFizOxr3tczBPspp5ABbEo8tiIZBg==:9fbfabe975fa5a7f
"""System information tool - exposes MCP runtime paths, time, and environment.

Builtin tool that runs in-process to provide system information
to the LLM about the MCP runtime environment.
"""

import os
import platform
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict

from rye.constants import AI_DIR

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/system"
__tool_description__ = (
    "System information tool - exposes MCP runtime paths, time, and environment"
)


def execute(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Execute system info query.

    Args:
        params: Contains 'item': "paths", "time", "runtime", or "all"
        project_path: Project root path from executor

    Returns:
        Dict with success, data, and optional error
    """
    item = params.get("item", "all")
    config = {"project_path": project_path}

    try:
        if item == "paths":
            data = _get_paths(config)
        elif item == "time":
            data = _get_time()
        elif item == "runtime":
            data = _get_runtime()
        elif item == "all":
            data = {
                "paths": _get_paths(config),
                "time": _get_time(),
                "runtime": _get_runtime(),
            }
        else:
            return {
                "success": False,
                "error": f"Unknown item: {item}. Valid: paths, time, runtime, all",
            }

        return {"success": True, "data": data}

    except Exception as e:
        return {"success": False, "error": str(e)}


def _get_paths(config: Dict[str, Any]) -> Dict[str, Any]:
    """Get filesystem paths relevant to the MCP."""
    project_path = config.get("project_path", os.getcwd())
    user_space = config.get(
        "user_space", os.environ.get("USER_SPACE", str(Path.home()))
    )
    system_space = config.get("system_space", _get_system_space())

    return {
        "project_path": project_path,
        "user_space": user_space,
        "user_space_exists": Path(user_space).exists(),
        "system_space": system_space,
        "system_space_exists": Path(system_space).exists() if system_space else False,
        "home_dir": str(Path.home()),
        "cwd": os.getcwd(),
    }


def _get_time() -> Dict[str, Any]:
    """Get current time info."""
    now = datetime.now(timezone.utc)
    return {
        "utc_iso": now.isoformat(),
        "utc_timestamp": int(now.timestamp()),
        "local_time": datetime.now().isoformat(),
        "timezone": time.tzname[0],
    }


def _get_runtime() -> Dict[str, Any]:
    """Get runtime environment info."""
    return {
        "platform": sys.platform,
        "arch": platform.machine(),
        "python_version": sys.version,
        "python_executable": sys.executable,
    }


def _get_system_space() -> str:
    """Get system space base path (where rye is installed)."""
    try:
        import rye

        if rye.__file__:
            return str(Path(rye.__file__).parent)
        import importlib.util

        spec = importlib.util.find_spec("rye")
        if spec and spec.origin:
            return str(Path(spec.origin).parent)
        return ""
    except ImportError:
        return ""
