"""GC data types — result objects, retention policies, and state tracking."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional


@dataclass
class PruneResult:
    """Result from Phase 1: cache and execution pruning."""
    cache_entries_deleted: int = 0
    cache_bytes_freed: int = 0
    executions_deleted: int = 0
    total_freed: int = 0

    def __iadd__(self, other: PruneResult) -> PruneResult:
        self.cache_entries_deleted += other.cache_entries_deleted
        self.cache_bytes_freed += other.cache_bytes_freed
        self.executions_deleted += other.executions_deleted
        self.total_freed += other.total_freed
        return self


@dataclass
class CompactionResult:
    """Result from Phase 2: history compaction for one project."""
    retained_count: int = 0
    discarded_count: int = 0
    new_head: Optional[str] = None
    old_head: Optional[str] = None
    dry_run: bool = False
    skipped: bool = False
    skip_reason: Optional[str] = None

    def to_dict(self) -> Dict[str, Any]:
        d: Dict[str, Any] = {
            "retained_count": self.retained_count,
            "discarded_count": self.discarded_count,
            "dry_run": self.dry_run,
            "skipped": self.skipped,
        }
        if self.new_head:
            d["new_head"] = self.new_head
        if self.old_head:
            d["old_head"] = self.old_head
        if self.skip_reason:
            d["skip_reason"] = self.skip_reason
        return d


@dataclass
class SweepResult:
    """Result from Phase 3: mark-and-sweep of unreachable objects."""
    deleted_objects: int = 0
    deleted_blobs: int = 0
    freed_bytes: int = 0
    dry_run: bool = False
    skipped: bool = False
    skip_reason: Optional[str] = None

    def to_dict(self) -> Dict[str, Any]:
        d: Dict[str, Any] = {
            "deleted_objects": self.deleted_objects,
            "deleted_blobs": self.deleted_blobs,
            "freed_bytes": self.freed_bytes,
            "dry_run": self.dry_run,
            "skipped": self.skipped,
        }
        if self.skip_reason:
            d["skip_reason"] = self.skip_reason
        return d


@dataclass
class GCResult:
    """Combined result from a full GC run."""
    prune: PruneResult
    compaction: Dict[str, CompactionResult]
    sweep: SweepResult
    total_freed_bytes: int = 0
    duration_ms: int = 0
    error: Optional[str] = None

    def to_dict(self) -> Dict[str, Any]:
        return {
            "prune": {
                "cache_entries_deleted": self.prune.cache_entries_deleted,
                "cache_bytes_freed": self.prune.cache_bytes_freed,
                "executions_deleted": self.prune.executions_deleted,
                "total_freed": self.prune.total_freed,
            },
            "compaction": {k: v.to_dict() for k, v in self.compaction.items()},
            "sweep": self.sweep.to_dict(),
            "total_freed_bytes": self.total_freed_bytes,
            "duration_ms": self.duration_ms,
            "error": self.error,
        }


@dataclass
class RetentionPolicy:
    """Per-project retention policy for history compaction."""
    manual_pushes: int = 3
    daily_checkpoints: int = 7
    weekly_checkpoints: int = 0
    max_success_executions: int = 10
    max_failure_executions: int = 10
    pinned: List[str] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "manual_pushes": self.manual_pushes,
            "daily_checkpoints": self.daily_checkpoints,
            "weekly_checkpoints": self.weekly_checkpoints,
            "max_success_executions": self.max_success_executions,
            "max_failure_executions": self.max_failure_executions,
            "pinned": self.pinned,
        }


DEFAULT_RETENTION = RetentionPolicy()


@dataclass
class GCState:
    """Persisted state for incremental GC runs."""
    last_gc_at: str = ""
    last_full_gc_at: str = ""
    reachable_hashes_blob: str = ""
    reachable_count: int = 0
    objects_at_last_gc: int = 0
    generation: int = 0
    invalidated: bool = False

    def to_dict(self) -> Dict[str, Any]:
        return {
            "last_gc_at": self.last_gc_at,
            "last_full_gc_at": self.last_full_gc_at,
            "reachable_hashes_blob": self.reachable_hashes_blob,
            "reachable_count": self.reachable_count,
            "objects_at_last_gc": self.objects_at_last_gc,
            "generation": self.generation,
            "invalidated": self.invalidated,
        }


@dataclass
class WriterEpoch:
    """Tracks an in-flight write operation for GC safety."""
    epoch_id: str = ""
    node_id: str = ""
    user_id: str = ""
    root_hashes: List[str] = field(default_factory=list)
    created_at: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "epoch_id": self.epoch_id,
            "node_id": self.node_id,
            "user_id": self.user_id,
            "root_hashes": self.root_hashes,
            "created_at": self.created_at,
        }


@dataclass
class DistributedGCLock:
    """Metadata for the mkdir-based distributed GC lock."""
    gc_run_id: str = ""
    node_id: str = ""
    started_at: str = ""
    expires_at: str = ""
    generation: int = 0
    phase: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "gc_run_id": self.gc_run_id,
            "node_id": self.node_id,
            "started_at": self.started_at,
            "expires_at": self.expires_at,
            "generation": self.generation,
            "phase": self.phase,
        }
