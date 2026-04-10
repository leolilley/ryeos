# rye:signed:2026-04-10T00:57:18Z:976501aafe6f56344ab85ae87a765e4b39e0083655b99c4acd3a9b188c7a71b0:fXX_zqouQXttsMLocWTbPgnvsOuo1PtCJ2JYxYe1diFF3kdI77U-amg9WJN7KqtE-X_VLrFTiEoWRcxj_A1UBQ:4b987fd4e40303ac
"""
CAS garbage collection tool — mark reachable objects, sweep unreachable ones.

Works identically locally and remotely. Targets the project's CAS at
``project_path / AI_DIR / "objects"``.

Actions:
  run      - Execute GC (mark + sweep)
  dry-run  - Preview what would be deleted
  status   - Show CAS usage stats
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/gc"
__execution__ = "routed"
__tool_description__ = "CAS garbage collection — mark reachable, sweep unreachable"

import logging
from pathlib import Path
from typing import Any, Dict

from rye.constants import AI_DIR

logger = logging.getLogger(__name__)

TOOL_METADATA = {
    "name": "gc",
    "description": "CAS garbage collection: mark reachable objects, sweep unreachable",
    "version": __version__,
    "protected": True,
}

ACTIONS = ["run", "dry-run", "status"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "GC operation to perform",
        },
        "aggressive": {
            "type": "boolean",
            "description": "Emergency mode — shorter grace window",
            "default": False,
        },
    },
    "required": ["action"],
}


def _human_bytes(n: int) -> str:
    v = float(n)
    for unit in ("B", "KB", "MB", "GB"):
        if abs(v) < 1024:
            return f"{v:.1f} {unit}"
        v /= 1024
    return f"{v:.1f} TB"


def _measure_usage(root: Path) -> int:
    total = 0
    try:
        for f in root.rglob("*"):
            if f.is_file():
                try:
                    total += f.stat().st_size
                except OSError:
                    pass
    except OSError:
        pass
    return total


async def execute(params: dict, project_path: str) -> dict:
    """Entry point — operates on the project's CAS."""
    action = params.get("action")
    if not action:
        return {"success": False, "error": "action required"}
    if action not in ACTIONS:
        return {"success": False, "error": f"Unknown action: {action}"}

    project = Path(project_path)
    cas_root = project / AI_DIR / "objects"

    if not cas_root.is_dir():
        return {"success": True, "message": "No CAS objects found"}

    try:
        if action == "status":
            return _status(cas_root)
        else:
            return _run(params, project, cas_root)
    except Exception as e:
        logger.exception("GC tool failed")
        return {"success": False, "error": f"GC failed: {e}"}


def _status(cas_root: Path) -> Dict[str, Any]:
    obj_count = (
        sum(1 for _ in (cas_root / "objects").rglob("*.json"))
        if (cas_root / "objects").is_dir()
        else 0
    )
    blob_count = (
        sum(1 for f in (cas_root / "blobs").rglob("*") if f.is_file())
        if (cas_root / "blobs").is_dir()
        else 0
    )
    total_bytes = _measure_usage(cas_root)

    ref_count = 0
    refs_dir = cas_root / "refs"
    if refs_dir.is_dir():
        ref_count = sum(1 for f in refs_dir.rglob("*") if f.is_file())

    return {
        "success": True,
        "objects": obj_count,
        "blobs": blob_count,
        "refs": ref_count,
        "total_bytes": total_bytes,
        "total_human": _human_bytes(total_bytes),
    }


def _run(params: Dict, project: Path, cas_root: Path) -> Dict[str, Any]:
    from rye.cas.gc import mark_reachable, sweep
    from rye.cas.gc_lock import acquire, release

    dry_run = params.get("action") == "dry-run"
    grace = 300 if params.get("aggressive") else 3600
    node_id = "local-gc-tool"

    lock = acquire(project, node_id)
    if lock is None:
        return {
            "success": False,
            "error": "GC lock held by another process",
        }

    try:
        reachable = mark_reachable(project, cas_root)

        obj_count = (
            sum(1 for _ in (cas_root / "objects").rglob("*.json"))
            if (cas_root / "objects").is_dir()
            else 0
        )
        blob_count = (
            sum(1 for f in (cas_root / "blobs").rglob("*") if f.is_file())
            if (cas_root / "blobs").is_dir()
            else 0
        )

        sweep_result = sweep(
            project, cas_root, reachable, grace_seconds=grace, dry_run=dry_run
        )
    finally:
        release(project, node_id)

    return {
        "success": True,
        "dry_run": dry_run,
        "total_objects": obj_count,
        "total_blobs": blob_count,
        "reachable_count": len(reachable),
        "unreachable_objects": sweep_result.deleted_objects,
        "unreachable_blobs": sweep_result.deleted_blobs,
        "freed_bytes": sweep_result.freed_bytes,
        "freed_human": _human_bytes(sweep_result.freed_bytes),
    }
