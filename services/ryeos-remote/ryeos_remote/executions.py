"""Local filesystem execution record tracking.

Replaces Supabase threads table with running files, append-only
execution log, and by-id index for O(1) lookup.
"""

import datetime
import json
import logging
import os
from pathlib import Path

logger = logging.getLogger(__name__)


def register_execution(
    cas_base: str,
    user_fp: str,
    thread_id: str,
    item_type: str,
    item_id: str,
    project_manifest_hash: str,
    user_manifest_hash: str | None,
    project_path: str | None,
    remote_name: str,
    system_version: str,
) -> None:
    try:
        running_dir = Path(cas_base) / user_fp / "running"
        running_dir.mkdir(parents=True, exist_ok=True)
        record = {
            "thread_id": thread_id,
            "user_id": user_fp,
            "item_type": item_type,
            "item_id": item_id,
            "execution_mode": "remote",
            "remote_name": remote_name,
            "project_path": project_path,
            "project_manifest_hash": project_manifest_hash,
            "user_manifest_hash": user_manifest_hash,
            "system_version": system_version,
            "state": "running",
            "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        }
        (running_dir / f"{thread_id}.json").write_text(json.dumps(record))
    except Exception:
        logger.warning("Failed to register execution %s", thread_id, exc_info=True)


def complete_execution(
    cas_base: str,
    user_fp: str,
    thread_id: str,
    state: str,
    snapshot_hash: str | None = None,
    runtime_outputs_bundle_hash: str | None = None,
    merge_conflicts: dict | None = None,
    unmerged_snapshot_hash: str | None = None,
) -> None:
    try:
        base = Path(cas_base) / user_fp
        running_file = base / "running" / f"{thread_id}.json"

        # Read base metadata from running file
        if running_file.exists():
            record = json.loads(running_file.read_text())
        else:
            record = {"thread_id": thread_id}

        completed_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
        record.update({
            "state": state,
            "completed_at": completed_at,
            "snapshot_hash": snapshot_hash,
            "runtime_outputs_bundle_hash": runtime_outputs_bundle_hash,
            "merge_conflicts": merge_conflicts,
            "unmerged_snapshot_hash": unmerged_snapshot_hash,
        })

        # Write by-id index
        by_id_dir = base / "executions" / "by-id"
        by_id_dir.mkdir(parents=True, exist_ok=True)
        (by_id_dir / thread_id).write_text(json.dumps(record))

        # Append to log
        log_dir = base / "logs"
        log_dir.mkdir(parents=True, exist_ok=True)
        log_entry = {
            "thread_id": thread_id,
            "state": state,
            "project_path": record.get("project_path"),
            "completed_at": completed_at,
            "snapshot_hash": snapshot_hash,
        }
        with open(log_dir / "executions.log", "a") as f:
            f.write(json.dumps(log_entry) + "\n")

        # Remove running file
        if running_file.exists():
            running_file.unlink()
    except Exception:
        logger.warning("Failed to complete execution %s", thread_id, exc_info=True)


def list_executions(
    cas_base: str,
    user_fp: str,
    project_path: str | None = None,
    limit: int = 20,
) -> list[dict]:
    base = Path(cas_base) / user_fp
    results: list[dict] = []

    # Collect in-flight executions from running dir
    running_dir = base / "running"
    if running_dir.is_dir():
        for f in running_dir.iterdir():
            if f.suffix == ".json":
                try:
                    rec = json.loads(f.read_text())
                    if project_path is None or rec.get("project_path") == project_path:
                        results.append(rec)
                except Exception:
                    logger.warning("Failed to read running file %s", f, exc_info=True)

    # Read completed executions from log (most recent first)
    log_file = base / "logs" / "executions.log"
    if log_file.is_file():
        try:
            lines = log_file.read_text().splitlines()
            for line in reversed(lines):
                if len(results) >= limit:
                    break
                if not line.strip():
                    continue
                try:
                    entry = json.loads(line)
                    if project_path is None or entry.get("project_path") == project_path:
                        results.append(entry)
                except json.JSONDecodeError:
                    continue
        except Exception:
            logger.warning("Failed to read execution log", exc_info=True)

    return results[:limit]


def get_execution(
    cas_base: str,
    user_fp: str,
    thread_id: str,
) -> dict | None:
    base = Path(cas_base) / user_fp

    # Check by-id index first
    by_id_file = base / "executions" / "by-id" / thread_id
    if by_id_file.is_file():
        try:
            return json.loads(by_id_file.read_text())
        except Exception:
            logger.warning("Failed to read by-id record %s", thread_id, exc_info=True)

    # Fall back to running dir
    running_file = base / "running" / f"{thread_id}.json"
    if running_file.is_file():
        try:
            return json.loads(running_file.read_text())
        except Exception:
            logger.warning("Failed to read running file %s", thread_id, exc_info=True)

    return None


def store_conflict_record(
    cas_base: str,
    user_fp: str,
    thread_id: str,
    conflicts: dict,
    unmerged_snapshot: str,
) -> None:
    try:
        by_id_file = Path(cas_base) / user_fp / "executions" / "by-id" / thread_id
        if by_id_file.is_file():
            record = json.loads(by_id_file.read_text())
            record["merge_conflicts"] = conflicts
            record["unmerged_snapshot_hash"] = unmerged_snapshot
            by_id_file.write_text(json.dumps(record))
        else:
            by_id_file.parent.mkdir(parents=True, exist_ok=True)
            record = {
                "thread_id": thread_id,
                "merge_conflicts": conflicts,
                "unmerged_snapshot_hash": unmerged_snapshot,
            }
            by_id_file.write_text(json.dumps(record))
    except Exception:
        logger.warning("Failed to store conflict record %s", thread_id, exc_info=True)
