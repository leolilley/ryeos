# rye:signed:2026-02-26T05:52:23Z:288bea602f593d6fd59de7a311476e9ae36408d19ed3751e0e646bd412ddeb15:qxoYveA6t-2fxnIvZk-z8s3ybRcrPdjwk8DCGSJKUz6zpIn7sCk4eDwjuKZqV_0zUpadT2jloUuIZPS7IYI1Cg==:4b987fd4e40303ac
"""Telemetry tool - exposes MCP server log reading and diagnostics.

Builtin tool that runs in-process to provide log inspection
for the RYE MCP server. For spawned agent telemetry, see agent/telemetry/.
"""

import os
import re
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/telemetry"
__tool_description__ = (
    "Telemetry tool - read and analyze RYE MCP server logs and diagnostics"
)

# Log level threshold — controls which entries are written by the MCP server.
# Set RYE_LOG_LEVEL=DEBUG|INFO|WARNING|ERROR|CRITICAL (default: INFO)
LOG_LEVEL = os.environ.get("RYE_LOG_LEVEL", "INFO").upper()

# Standard Python logging format: 2026-02-10 12:34:56,789 - name - LEVEL - message
_STANDARD_RE = re.compile(
    r"^(\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2},\d{3})\s+-\s+\S+\s+-\s+"
    r"(DEBUG|INFO|WARNING|ERROR|CRITICAL)\s+-\s+(.*)"
)

# Simple format: LEVEL: message
_SIMPLE_RE = re.compile(
    r"^(DEBUG|INFO|WARNING|ERROR|CRITICAL):\s+(.*)"
)

VALID_LEVELS = {"DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"}


def execute(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Execute telemetry query.

    Args:
        params: Contains 'item': "logs", "stats", "errors", or "all"
                Optional: 'level' (str), 'limit' (int)
        project_path: Project root path from executor

    Returns:
        Dict with success, data, and optional error
    """
    item = params.get("item", "all")
    level = params.get("level", LOG_LEVEL)
    limit = params.get("limit", 50)

    # Derive log dir from USER_SPACE env → ~
    user_space = os.environ.get("USER_SPACE", str(Path.home()))
    default_log_dir = Path(user_space) / AI_DIR / "logs" / "rye"
    log_dir = Path(params.get("log_dir", str(default_log_dir)))

    try:
        if item == "logs":
            data = _get_logs(log_dir, level=level, limit=limit)
        elif item == "stats":
            data = _get_stats(log_dir)
        elif item == "errors":
            data = _get_errors(log_dir)
        elif item == "all":
            data = {
                "logs": _get_logs(log_dir, level=level, limit=limit),
                "stats": _get_stats(log_dir),
                "errors": _get_errors(log_dir),
            }
        else:
            return {
                "success": False,
                "error": f"Unknown item: {item}. Valid: logs, stats, errors, all",
            }

        return {"success": True, "data": data}

    except Exception as e:
        return {"success": False, "error": str(e)}


def _parse_line(line: str) -> Optional[Dict[str, str]]:
    """Parse a single log line into a structured dict.

    Tries standard Python logging format first, then simple LEVEL: message,
    and falls back to UNKNOWN level.
    """
    line = line.rstrip("\n\r")
    if not line.strip():
        return None

    m = _STANDARD_RE.match(line)
    if m:
        return {
            "timestamp": m.group(1),
            "level": m.group(2),
            "message": m.group(3),
        }

    m = _SIMPLE_RE.match(line)
    if m:
        return {
            "timestamp": "",
            "level": m.group(1),
            "message": m.group(2),
        }

    return {
        "timestamp": "",
        "level": "UNKNOWN",
        "message": line.strip(),
    }


def _read_all_entries(log_dir: Path) -> List[Dict[str, str]]:
    """Read and parse all log entries from all log files in the directory."""
    entries: List[Dict[str, str]] = []
    if not log_dir.exists():
        return entries

    log_files = sorted(log_dir.glob("*.log"), key=lambda f: f.stat().st_mtime)
    for lf in log_files:
        try:
            with open(lf, "r", errors="replace") as fh:
                for line in fh:
                    parsed = _parse_line(line)
                    if parsed is not None:
                        entries.append(parsed)
        except OSError:
            continue

    return entries


def _get_logs(
    log_dir: Path,
    level: Optional[str] = None,
    limit: int = 50,
) -> Dict[str, Any]:
    """Get recent log entries, optionally filtered by level."""
    if not log_dir.exists():
        return {
            "entries": [],
            "total": 0,
            "log_dir": str(log_dir),
            "exists": False,
        }

    entries = _read_all_entries(log_dir)

    if level:
        level_upper = level.upper()
        entries = [e for e in entries if e["level"] == level_upper]

    total = len(entries)
    entries = entries[-limit:]

    return {
        "entries": entries,
        "total": total,
        "returned": len(entries),
        "log_dir": str(log_dir),
        "exists": True,
    }


def _get_stats(log_dir: Path) -> Dict[str, Any]:
    """Get log file statistics: counts, sizes, per-level counts, timestamps."""
    if not log_dir.exists():
        return {
            "exists": False,
            "log_dir": str(log_dir),
        }

    log_files = list(log_dir.glob("*.log"))
    total_size = sum(f.stat().st_size for f in log_files)

    level_counts: Dict[str, int] = {}
    oldest_ts: Optional[str] = None
    newest_ts: Optional[str] = None

    for lf in sorted(log_files, key=lambda f: f.stat().st_mtime):
        try:
            with open(lf, "r", errors="replace") as fh:
                for line in fh:
                    parsed = _parse_line(line)
                    if parsed is None:
                        continue
                    lvl = parsed["level"]
                    level_counts[lvl] = level_counts.get(lvl, 0) + 1
                    ts = parsed["timestamp"]
                    if ts:
                        if oldest_ts is None:
                            oldest_ts = ts
                        newest_ts = ts
        except OSError:
            continue

    return {
        "exists": True,
        "log_dir": str(log_dir),
        "file_count": len(log_files),
        "total_size_bytes": total_size,
        "level_counts": level_counts,
        "oldest_entry": oldest_ts,
        "newest_entry": newest_ts,
    }


def _get_errors(log_dir: Path) -> Dict[str, Any]:
    """Convenience shortcut: ERROR and WARNING entries, last 20."""
    if not log_dir.exists():
        return {
            "entries": [],
            "total": 0,
            "log_dir": str(log_dir),
            "exists": False,
        }

    entries = _read_all_entries(log_dir)
    entries = [e for e in entries if e["level"] in ("ERROR", "WARNING")]

    total = len(entries)
    entries = entries[-20:]

    return {
        "entries": entries,
        "total": total,
        "returned": len(entries),
        "log_dir": str(log_dir),
        "exists": True,
    }
