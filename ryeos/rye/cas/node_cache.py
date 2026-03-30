"""Node execution cache — skip re-execution when inputs haven't changed.

Opt-in per node via ``cache: true`` in graph YAML.
Cache keys are deterministic hashes of interpolated action + config state.
"""

from __future__ import annotations

import hashlib
import json
import logging
import time
from pathlib import Path
from typing import Any, Dict, Optional

from rye.primitives import cas
from rye.cas.objects import NodeInput, NodeResult

logger = logging.getLogger(__name__)


def compute_cache_key(
    graph_hash: str,
    node_name: str,
    interpolated_action: Dict[str, Any],
    lockfile_hash: Optional[str],
    config_snapshot_hash: str,
) -> str:
    """Compute deterministic cache key for a node execution.

    Returns SHA256 hex digest.
    """
    node_input = NodeInput(
        graph_hash=graph_hash,
        node_name=node_name,
        interpolated_action=interpolated_action,
        lockfile_hash=lockfile_hash,
        config_snapshot_hash=config_snapshot_hash,
    )
    canonical = json.dumps(
        node_input.to_dict(), sort_keys=True, separators=(",", ":"), default=str,
    )
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def cache_lookup(
    cache_key: str,
    project_path: Path,
) -> Optional[Dict[str, Any]]:
    """Check cache for a node result.

    Returns {"result": ..., "node_result_hash": ...} on hit, None on miss.
    """
    cache_dir = project_path / ".ai" / "objects" / "cache" / "nodes"
    cache_file = cache_dir / f"{cache_key}.json"

    if not cache_file.exists():
        return None

    try:
        data = json.loads(cache_file.read_text())
        result_hash = data.get("node_result_hash")
        if not result_hash:
            return None

        root = project_path / ".ai" / "objects"
        result_obj = cas.get_object(result_hash, root)
        if result_obj is None:
            return None

        return {"result": result_obj.get("result"), "node_result_hash": result_hash}
    except Exception:
        logger.debug("Cache lookup failed for %s", cache_key, exc_info=True)
        return None


def cache_store(
    cache_key: str,
    result: Dict[str, Any],
    project_path: Path,
    node_name: str,
    elapsed_ms: int,
) -> Optional[str]:
    """Store a node result in the cache.

    Returns the node_result object hash, or None on failure.
    """
    try:
        root = project_path / ".ai" / "objects"

        # Store NodeResult as CAS object
        node_result = NodeResult(result=result)
        result_hash = cas.store_object(node_result.to_dict(), root)

        # Write cache pointer
        cache_dir = project_path / ".ai" / "objects" / "cache" / "nodes"
        cache_dir.mkdir(parents=True, exist_ok=True)
        cache_file = cache_dir / f"{cache_key}.json"
        cache_file.write_text(json.dumps({
            "node_result_hash": result_hash,
            "node_name": node_name,
            "cached_at": time.time(),
        }))

        return result_hash
    except Exception:
        logger.debug("Cache store failed for %s", cache_key, exc_info=True)
        return None
