# rye:signed:2026-03-30T04:30:49Z:c4fa6373862242ad76e29a77df11e41776ea8377e99d3d7fbf8c6f9070d49b31:rhcpQ0MJq_9EKw8jXLKAxG99ekV4UmsnEpKRLmbXuNDNlG6zq9nTs-AxR_H0CuOn_h-KugPz68BnqGup04pzAA:4b987fd4e40303ac
"""
Reference routing tool — capability-based dispatch to cluster nodes.

Queries /status on known remotes, matches capabilities, selects the
least-loaded healthy provider, and dispatches execution.

Anti-loop: only dispatches to nodes that PROVIDE the capability,
never to nodes that ROUTE it (preventing routing loops).

Override this tool via project/user space for custom routing policies.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/remote/route"
__execution__ = "routed"  # This tool routes, it doesn't provide
__tool_description__ = "Capability-based routing to cluster nodes"

import fnmatch
import json
import logging
import random
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

TOOL_METADATA = {
    "name": "route",
    "description": "Route execution to capable cluster nodes",
    "version": __version__,
    "protected": True,
}

ACTIONS = ["route"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {"type": "string", "enum": ACTIONS},
        "capability": {"type": "string", "description": "Capability pattern to match (fnmatch)"},
        "item_type": {"type": "string", "description": "tool or directive"},
        "item_id": {"type": "string", "description": "Item to execute"},
        "parameters": {"type": "object", "description": "Execution parameters"},
        "thread": {"type": "string", "description": "Execution thread mode"},
    },
    "required": ["action", "capability", "item_type", "item_id"],
}


def _load_topology(project_path: Optional[str] = None) -> Dict:
    """Load cluster/topology.yaml via 3-tier resolution."""
    from rye.cas.manifest import _load_config_3tier
    from pathlib import Path
    config = _load_config_3tier("cluster/topology.yaml", Path(project_path) if project_path else None)
    return config.get("routing", {})


class _SimpleClient:
    """Minimal HTTP client for /status queries (no auth needed)."""
    def __init__(self, base_url: str, timeout: int = 10):
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self._http = None

    async def get(self, path: str) -> dict:
        if self._http is None:
            from rye.runtime.http_client import HttpClientPrimitive
            self._http = HttpClientPrimitive()
        config = {
            "method": "GET",
            "url": f"{self.base_url}{path}",
            "headers": {"Content-Type": "application/json"},
            "timeout": self.timeout,
        }
        result = await self._http.execute(config, {})
        return {
            "success": result.success,
            "status_code": result.status_code,
            "body": result.body,
            "error": result.error,
        }


async def _route(params: Dict, project_path: str) -> Dict:
    """Route execution to the best available node."""
    capability = params.get("capability")
    item_type = params.get("item_type")
    item_id = params.get("item_id")
    exec_params = params.get("parameters", {})
    thread = params.get("thread")

    if not capability or not item_type or not item_id:
        return {"error": "Required: capability, item_type, item_id"}

    if not thread:
        thread = "fork" if item_type == "directive" else "inline"

    # Load routing policy
    topology = _load_topology(project_path)
    strategy = topology.get("strategy", "least-loaded")
    load_threshold = topology.get("load_threshold", 0.9)
    timeout = topology.get("timeout_seconds", 10)
    status_ttl = topology.get("status_ttl_seconds", 30)

    # Load known remotes
    from remote_config import list_remotes
    from pathlib import Path

    remotes = list_remotes(Path(project_path) if project_path else None)
    if not remotes:
        return {"error": "No remotes configured in cas/remote.yaml"}

    # Query /status on all remotes (cached)
    from status_cache import StatusCache
    cache = StatusCache(ttl=status_ttl)

    clients = {}
    for name, info in remotes.items():
        url = info.get("url", "")
        if url:
            clients[name] = _SimpleClient(url, timeout=timeout)

    if not clients:
        return {"error": "No reachable remotes configured"}

    statuses = await cache.get_cluster_status(clients)

    # Filter: healthy nodes that PROVIDE (not route) the capability
    candidates = []
    for name, status in statuses.items():
        if not status.healthy:
            continue
        # Anti-loop: only match against 'provides', never 'routes'
        for provided in status.provides:
            if fnmatch.fnmatch(provided, capability) or fnmatch.fnmatch(capability, provided):
                candidates.append((name, status))
                break

    if not candidates:
        return {
            "error": f"No healthy node provides capability: {capability}",
            "checked_nodes": list(statuses.keys()),
            "unhealthy": [n for n, s in statuses.items() if not s.healthy],
        }

    # Filter by load threshold
    if load_threshold < 1.0:
        candidates = [
            (name, status) for name, status in candidates
            if status.max_concurrent == 0 or (status.active / status.max_concurrent) < load_threshold
        ]
        if not candidates:
            return {
                "error": f"All capable nodes above load threshold ({load_threshold})",
                "checked_nodes": list(statuses.keys()),
            }

    # Rank by strategy
    if strategy == "round-robin":
        random.shuffle(candidates)
    else:  # least-loaded (default, also covers affinity for now)
        candidates.sort(key=lambda c: (c[1].active, random.random()))

    selected_name, selected_status = candidates[0]

    logger.info(
        "Routing %s to %s (active=%d/%d)",
        capability, selected_name,
        selected_status.active, selected_status.max_concurrent,
    )

    # Dispatch via remote tool execute action
    from remote import execute as remote_execute

    result = await remote_execute(
        {
            "action": "execute",
            "remote": selected_name,
            "item_type": item_type,
            "item_id": item_id,
            "parameters": exec_params,
            "thread": thread,
        },
        project_path,
    )

    result["routed_to"] = selected_name
    result["routed_node_id"] = selected_status.node_id
    return result


async def execute(params: dict, project_path: str) -> dict:
    """Entry point for function runtime."""
    action = params.pop("action", None)
    if not action:
        return {"success": False, "error": "action required"}
    if action != "route":
        return {"success": False, "error": f"Unknown action: {action}"}

    try:
        result = await _route(params, project_path)
    except Exception as e:
        logger.exception("Route failed")
        result = {"error": f"Routing failed: {e}"}

    if "error" in result:
        result["success"] = False
    elif "success" not in result:
        result["success"] = True
    return result
