# rye:signed:2026-02-12T23:55:37Z:2d1f1b78a11d61061706fcf9d795d1f98ee0d89a8dfe1f515697ca76200f965f:dN69OCqkwomAGNkkaiyoUieC-snpPA6vFSfCKKyRjLAkMtnveIlFrgOdM5e6EL9tTgQq2TlKJFY7GOAvKnu8AA==:440443d0858f0199
"""
MCP Server Logs Tool

Provides access to the RYE MCP server's own logging and diagnostics.
This is for inspecting the MCP server itself, NOT spawned agents.
For spawned agent telemetry, see agent/telemetry/.
"""

__version__ = "1.0.0"
__tool_type__ = "telemetry"
__category__ = "rye/core/telemetry"
__tool_description__ = (
    "MCP server logs tool - access RYE MCP server logging and diagnostics"
)

import logging
from pathlib import Path
from typing import Optional, List, Dict, Any
from datetime import datetime, timezone

from rye.constants import AI_DIR

# Default log location - can be overridden
DEFAULT_LOG_DIR = Path.home() / AI_DIR / "logs" / "rye"


async def get_logs(
    level: Optional[str] = None,
    since: Optional[str] = None,
    limit: int = 100,
    log_dir: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Get recent MCP server logs.

    Args:
        level: Filter by log level (DEBUG, INFO, WARNING, ERROR)
        since: ISO timestamp - only logs after this time
        limit: Maximum number of log entries to return
        log_dir: Override default log directory

    Returns:
        Dict with log entries and metadata
    """
    logs_path = Path(log_dir) if log_dir else DEFAULT_LOG_DIR

    if not logs_path.exists():
        return {
            "entries": [],
            "total": 0,
            "log_dir": str(logs_path),
            "exists": False,
        }

    # TODO: Implement log parsing based on actual log format
    # For now, return placeholder
    return {
        "entries": [],
        "total": 0,
        "log_dir": str(logs_path),
        "exists": True,
        "note": "Log parsing not yet implemented - check actual log format",
    }


async def get_log_stats(log_dir: Optional[str] = None) -> Dict[str, Any]:
    """
    Get statistics about MCP server logs.

    Returns:
        Dict with log statistics (counts by level, size, etc.)
    """
    logs_path = Path(log_dir) if log_dir else DEFAULT_LOG_DIR

    if not logs_path.exists():
        return {
            "exists": False,
            "log_dir": str(logs_path),
        }

    # Count log files
    log_files = list(logs_path.glob("*.log"))
    total_size = sum(f.stat().st_size for f in log_files)

    return {
        "exists": True,
        "log_dir": str(logs_path),
        "file_count": len(log_files),
        "total_size_bytes": total_size,
        "files": [f.name for f in log_files[:10]],  # First 10 files
    }
