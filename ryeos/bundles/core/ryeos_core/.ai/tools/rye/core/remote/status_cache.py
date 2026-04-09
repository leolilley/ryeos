# rye:signed:2026-04-09T00:59:36Z:4c7ee136f4284dc984133751387d3d6f2d0eaefe285fb7f61fae9c483ae8d8a8:rQ9XddA6fJ7LRnk13t3Ajvj3BkFlLHrWGZo0HlPiG335wjbWMmtjdOXY7cUQQp46Pbf8veijTtGRYafjC3GYDA:4b987fd4e40303ac
"""Client-side status cache for multi-node routing.

In-memory TTL cache for /status responses from ryeos-node servers.
Importable by routing tools — not a standalone tool.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/remote"
__tool_description__ = "Status cache for multi-node routing"

import logging
import time
from dataclasses import dataclass, field
from typing import Dict, List, Optional

logger = logging.getLogger(__name__)

DEFAULT_TTL_SECONDS = 30
UNHEALTHY_TTL_SECONDS = 10  # re-check unhealthy nodes faster


@dataclass
class CachedStatus:
    """Cached /status response from a node."""
    node_id: str
    node_name: str
    healthy: bool
    active: int
    max_concurrent: int
    provides: List[str] = field(default_factory=list)
    routes: List[str] = field(default_factory=list)
    fetched_at: float = 0.0
    error: Optional[str] = None


class StatusCache:
    """TTL-based cache for node /status responses.
    
    Usage:
        cache = StatusCache(ttl=30)
        status = await cache.get_status("default", client)
        cluster = await cache.get_cluster_status(["default", "gpu"], clients)
    """
    
    def __init__(self, ttl: float = DEFAULT_TTL_SECONDS):
        self.ttl = ttl
        self._cache: Dict[str, CachedStatus] = {}
    
    def _is_fresh(self, entry: CachedStatus) -> bool:
        age = time.monotonic() - entry.fetched_at
        if entry.healthy:
            return age < self.ttl
        return age < UNHEALTHY_TTL_SECONDS
    
    def get_cached(self, node_name: str) -> Optional[CachedStatus]:
        """Get cached status if fresh, else None."""
        entry = self._cache.get(node_name)
        if entry and self._is_fresh(entry):
            return entry
        return None
    
    async def fetch_status(self, node_name: str, client) -> CachedStatus:
        """Fetch /status from a node and cache the result.
        
        Args:
            node_name: Name of the remote node
            client: HTTP client with async get() method returning {"success": bool, "body": dict}
        """
        try:
            resp = await client.get("/status")
            if not resp.get("success"):
                entry = CachedStatus(
                    node_id="",
                    node_name=node_name,
                    healthy=False,
                    active=0,
                    max_concurrent=0,
                    fetched_at=time.monotonic(),
                    error=resp.get("error", "Failed to fetch /status"),
                )
                self._cache[node_name] = entry
                return entry
            
            body = resp.get("body", {})
            if isinstance(body, str):
                import json
                body = json.loads(body)
            
            caps = body.get("capabilities", {})
            entry = CachedStatus(
                node_id=body.get("node_id", ""),
                node_name=node_name,
                healthy=body.get("healthy", False),
                active=body.get("active", 0),
                max_concurrent=body.get("max_concurrent", 0),
                provides=caps.get("provides", []),
                routes=caps.get("routes", []),
                fetched_at=time.monotonic(),
            )
            self._cache[node_name] = entry
            return entry
        except Exception as e:
            entry = CachedStatus(
                node_id="",
                node_name=node_name,
                healthy=False,
                active=0,
                max_concurrent=0,
                fetched_at=time.monotonic(),
                error=str(e),
            )
            self._cache[node_name] = entry
            return entry
    
    async def get_status(self, node_name: str, client) -> CachedStatus:
        """Get status from cache or fetch if stale."""
        cached = self.get_cached(node_name)
        if cached:
            return cached
        return await self.fetch_status(node_name, client)
    
    async def get_cluster_status(
        self,
        nodes: Dict[str, object],
    ) -> Dict[str, CachedStatus]:
        """Get status for multiple nodes.
        
        Args:
            nodes: Dict of node_name -> client
            
        Returns:
            Dict of node_name -> CachedStatus
        """
        import asyncio
        
        async def _fetch(name, client):
            return name, await self.get_status(name, client)
        
        tasks = [_fetch(name, client) for name, client in nodes.items()]
        results = await asyncio.gather(*tasks, return_exceptions=True)
        
        statuses = {}
        for r in results:
            if isinstance(r, Exception):
                continue
            name, status = r
            statuses[name] = status
        return statuses
    
    def mark_unhealthy(self, node_name: str, error: str = "marked unhealthy") -> None:
        """Mark a node as unhealthy (e.g., after a dispatch failure)."""
        entry = self._cache.get(node_name)
        if entry:
            entry.healthy = False
            entry.error = error
            entry.fetched_at = time.monotonic()
        else:
            self._cache[node_name] = CachedStatus(
                node_id="",
                node_name=node_name,
                healthy=False,
                active=0,
                max_concurrent=0,
                fetched_at=time.monotonic(),
                error=error,
            )
    
    def invalidate(self, node_name: str) -> None:
        """Remove cached status for a node."""
        self._cache.pop(node_name, None)
    
    def clear(self) -> None:
        """Clear entire cache."""
        self._cache.clear()
