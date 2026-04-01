# rye:signed:2026-04-01T01:03:42Z:b309afb7bae90c9220d170804de9faba12d90f0a57b45f995be1a5e42003669a:yGr8pod2Ow2RpskKZ2YpPjGjriOaJoIX-bZt_3plP4_NTWDuHiNPXlzragWzqXCdT-HHzbT9OgRYgpaFTD0dCQ:4b987fd4e40303ac
"""
CAS garbage collection tool — prune caches, compact history, sweep unreachable objects.

Actions:
  run      - Execute full GC (Phase 1→2→3)
  dry-run  - Preview what GC would delete without changing anything
  status   - Show current GC state, usage, and lock info
  history  - Show recent GC events
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/gc"
__execution__ = "routed"
__tool_description__ = "CAS garbage collection — prune, compact, sweep"

import json
import logging
from pathlib import Path
from typing import Any, Dict, List

from rye.constants import AI_DIR

logger = logging.getLogger(__name__)

TOOL_METADATA = {
    "name": "gc",
    "description": "CAS garbage collection: prune caches, compact history, sweep unreachable objects",
    "version": __version__,
    "protected": True,
}

ACTIONS = ["run", "dry-run", "status", "history"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "GC operation to perform",
        },
        "target": {
            "type": "string",
            "description": "User fingerprint or 'all' (default: all)",
            "default": "all",
        },
        "retention_days": {
            "type": "integer",
            "description": "Keep 1 daily checkpoint for this many days",
            "default": 7,
        },
        "max_manual_pushes": {
            "type": "integer",
            "description": "Keep last N manual push snapshots",
            "default": 3,
        },
        "max_executions": {
            "type": "integer",
            "description": "Keep last N success + N failure per graph",
            "default": 10,
        },
        "cache_max_age_hours": {
            "type": "integer",
            "description": "Delete cache snapshots older than this (hours)",
            "default": 24,
        },
        "aggressive": {
            "type": "boolean",
            "description": "Emergency mode — delete all caches, compact to HEAD only",
            "default": False,
        },
        "limit": {
            "type": "integer",
            "description": "Number of history events to show (history action)",
            "default": 10,
        },
    },
    "required": ["action"],
}


def _find_user_roots(cas_base: Path, target: str) -> List[Path]:
    """Resolve target to list of user root directories."""
    if not cas_base.is_dir():
        return []
    if target and target != "all":
        user_root = cas_base / target
        return [user_root] if user_root.is_dir() else []
    return [d for d in cas_base.iterdir() if d.is_dir() and not d.name.startswith(".")]


def _measure_usage(user_root: Path) -> int:
    """Sum all file sizes under user_root."""
    total = 0
    try:
        for f in user_root.rglob("*"):
            if f.is_file():
                try:
                    total += f.stat().st_size
                except OSError:
                    pass
    except OSError:
        pass
    return total


def _human_bytes(n: int) -> str:
    v = float(n)
    for unit in ("B", "KB", "MB", "GB"):
        if abs(v) < 1024:
            return f"{v:.1f} {unit}"
        v /= 1024
    return f"{v:.1f} TB"


async def _run_gc(params: Dict, cas_base: Path) -> Dict:
    """Execute or dry-run GC across target user roots."""
    from rye.cas.gc import run_gc
    from rye.cas.gc_types import RetentionPolicy

    dry_run = params.get("action") == "dry-run"
    aggressive = params.get("aggressive", False)
    target = params.get("target", "all")

    policy = RetentionPolicy(
        manual_pushes=params.get("max_manual_pushes", 3),
        daily_checkpoints=params.get("retention_days", 7),
        max_success_executions=params.get("max_executions", 10),
        max_failure_executions=params.get("max_executions", 10),
    )

    user_roots = _find_user_roots(cas_base, target)
    if not user_roots:
        return {"success": True, "message": "No user roots found", "results": {}}

    results: Dict[str, Any] = {}
    total_freed = 0

    for user_root in user_roots:
        cas_root = user_root / AI_DIR / "objects"
        if not cas_root.is_dir():
            continue
        result = run_gc(
            user_root,
            cas_root,
            dry_run=dry_run,
            aggressive=aggressive,
            policy=policy,
        )
        results[user_root.name] = result.to_dict()
        total_freed += result.total_freed_bytes

    return {
        "success": True,
        "dry_run": dry_run,
        "users_processed": len(results),
        "total_freed_bytes": total_freed,
        "total_freed_human": _human_bytes(total_freed),
        "results": results,
    }


async def _status(params: Dict, cas_base: Path) -> Dict:
    """Return GC state and usage for target user roots."""
    from rye.cas.gc_incremental import load_gc_state
    from rye.cas.gc_lock import read_lock

    target = params.get("target", "all")
    user_roots = _find_user_roots(cas_base, target)

    statuses: Dict[str, Any] = {}
    for user_root in user_roots:
        usage = _measure_usage(user_root)
        gc_state = load_gc_state(user_root)
        lock = read_lock(user_root)

        inflight_count = 0
        inflight_dir = user_root / "inflight"
        if inflight_dir.is_dir():
            inflight_count = sum(1 for f in inflight_dir.iterdir() if f.is_file())

        statuses[user_root.name] = {
            "usage_bytes": usage,
            "usage_human": _human_bytes(usage),
            "gc_state": gc_state.to_dict() if gc_state else None,
            "gc_lock": lock.to_dict() if lock else None,
            "inflight_epochs": inflight_count,
        }

    return {"success": True, "statuses": statuses}


async def _history(params: Dict, cas_base: Path) -> Dict:
    """Return recent GC events from the log."""
    target = params.get("target", "all")
    limit = params.get("limit", 10)
    user_roots = _find_user_roots(cas_base, target)

    all_events: Dict[str, List] = {}
    for user_root in user_roots:
        gc_log = user_root / "logs" / "gc.jsonl"
        if not gc_log.is_file():
            continue
        try:
            lines = gc_log.read_text(encoding="utf-8").strip().split("\n")
            events = []
            for line in lines[-limit:]:
                if line.strip():
                    try:
                        events.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
            if events:
                all_events[user_root.name] = events
        except OSError:
            continue

    return {"success": True, "events": all_events}


async def execute(params: dict, project_path: str) -> dict:
    """Entry point for function runtime."""
    action = params.get("action")
    if not action:
        return {"success": False, "error": "action required"}
    if action not in ACTIONS:
        return {"success": False, "error": f"Unknown action: {action}"}

    # Resolve CAS base path — check env, then fall back to conventional location
    import os

    cas_base_str = os.environ.get("CAS_BASE_PATH", "/cas")
    cas_base = Path(cas_base_str)

    try:
        if action in ("run", "dry-run"):
            return await _run_gc(params, cas_base)
        elif action == "status":
            return await _status(params, cas_base)
        elif action == "history":
            return await _history(params, cas_base)
        else:
            return {"success": False, "error": f"Unknown action: {action}"}
    except Exception as e:
        logger.exception("GC tool failed")
        return {"success": False, "error": f"GC failed: {e}"}
