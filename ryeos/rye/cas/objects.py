"""CAS object model — dataclasses for all object kinds.

Every object includes schema version for future evolution.
All hashing uses compute_integrity() (canonical JSON → SHA256).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional


SCHEMA_VERSION = 1


# --- Core refs ---


@dataclass(frozen=True)
class ItemRef:
    """Return type from ingest_item — references into CAS."""

    blob_hash: str
    object_hash: str
    integrity: str
    signature_info: Optional[Dict[str, str]]


# --- Object kinds ---


@dataclass(frozen=True)
class ItemSource:
    """Versioned snapshot of a signed or unsigned .ai/ file.

    signature_info is None for unsigned files (lockfiles, PEM keys).
    integrity is always populated (SHA256 of content blob).
    """

    kind: str = field(default="item_source", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    item_type: str = ""
    item_id: str = ""
    content_blob_hash: str = ""
    integrity: str = ""
    signature_info: Optional[Dict[str, str]] = None

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": self.schema,
            "kind": self.kind,
            "item_type": self.item_type,
            "item_id": self.item_id,
            "content_blob_hash": self.content_blob_hash,
            "integrity": self.integrity,
            "signature_info": self.signature_info,
        }


@dataclass(frozen=True)
class SourceManifest:
    """Filesystem closure — everything needed to materialize a space."""

    kind: str = field(default="source_manifest", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    space: str = ""
    items: Dict[str, str] = field(default_factory=dict)
    files: Dict[str, str] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": self.schema,
            "kind": self.kind,
            "space": self.space,
            "items": self.items,
            "files": self.files,
        }


@dataclass(frozen=True)
class ConfigSnapshot:
    """Merged config state after 3-tier resolution."""

    kind: str = field(default="config_snapshot", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    resolved_config: Dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": self.schema,
            "kind": self.kind,
            "resolved_config": self.resolved_config,
        }


@dataclass(frozen=True)
class NodeInput:
    """Cache key for node execution — must be deterministic."""

    kind: str = field(default="node_input", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    graph_hash: str = ""
    node_name: str = ""
    interpolated_action: Dict[str, Any] = field(default_factory=dict)
    lockfile_hash: Optional[str] = None
    config_snapshot_hash: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": self.schema,
            "kind": self.kind,
            "graph_hash": self.graph_hash,
            "node_name": self.node_name,
            "interpolated_action": self.interpolated_action,
            "lockfile_hash": self.lockfile_hash,
            "config_snapshot_hash": self.config_snapshot_hash,
        }


@dataclass(frozen=True)
class NodeResult:
    """Cached execution output — stores full unwrapped result dict."""

    kind: str = field(default="node_result", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    result: Dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": self.schema,
            "kind": self.kind,
            "result": self.result,
        }


@dataclass(frozen=True)
class NodeReceipt:
    """Audit record for a single node execution."""

    kind: str = field(default="node_receipt", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    node_input_hash: str = ""
    node_result_hash: str = ""
    cache_hit: bool = False
    elapsed_ms: int = 0
    timestamp: str = ""
    error: Optional[str] = None

    def to_dict(self) -> Dict[str, Any]:
        d = {
            "schema": self.schema,
            "kind": self.kind,
            "node_input_hash": self.node_input_hash,
            "node_result_hash": self.node_result_hash,
            "cache_hit": self.cache_hit,
            "elapsed_ms": self.elapsed_ms,
            "timestamp": self.timestamp,
        }
        if self.error is not None:
            d["error"] = self.error
        return d


@dataclass(frozen=True)
class ExecutionSnapshot:
    """Immutable run checkpoint."""

    kind: str = field(default="execution_snapshot", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    graph_run_id: str = ""
    graph_id: str = ""
    project_manifest_hash: str = ""
    user_manifest_hash: str = ""
    system_version: str = ""
    step: int = 0
    status: str = ""
    state_hash: str = ""
    node_receipts: List[str] = field(default_factory=list)
    errors: List[Dict[str, Any]] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        d = {
            "schema": self.schema,
            "kind": self.kind,
            "graph_run_id": self.graph_run_id,
            "graph_id": self.graph_id,
            "project_manifest_hash": self.project_manifest_hash,
            "user_manifest_hash": self.user_manifest_hash,
            "system_version": self.system_version,
            "step": self.step,
            "status": self.status,
            "state_hash": self.state_hash,
            "node_receipts": self.node_receipts,
        }
        if self.errors:
            d["errors"] = self.errors
        return d


@dataclass(frozen=True)
class StateSnapshot:
    """Graph state at a point in time."""

    kind: str = field(default="state_snapshot", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    state: Dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": self.schema,
            "kind": self.kind,
            "state": self.state,
        }


@dataclass(frozen=True)
class ArtifactIndex:
    """Per-thread call_id → blob_hash mapping for artifact retrieval."""

    kind: str = field(default="artifact_index", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    thread_id: str = ""
    entries: Dict[str, Dict[str, str]] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": self.schema,
            "kind": self.kind,
            "thread_id": self.thread_id,
            "entries": self.entries,
        }


@dataclass(frozen=True)
class RuntimeOutputsBundle:
    """Maps runtime-produced files to CAS blobs for remote output sync.

    After remote execution, runtime files (transcripts, thread.json,
    capabilities.md, knowledge markdown, refs) are stored as CAS blobs.
    This object records the mapping so clients can materialize them
    back into the local project tree.

    files: {relative_path_from_project_root: blob_hash}
      e.g. ".ai/agent/graphs/run-123/transcript.jsonl" → "abc123..."
    """

    kind: str = field(default="runtime_outputs_bundle", init=False)
    schema: int = field(default=SCHEMA_VERSION, init=False)
    remote_thread_id: str = ""
    execution_snapshot_hash: str = ""
    files: Dict[str, str] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": self.schema,
            "kind": self.kind,
            "remote_thread_id": self.remote_thread_id,
            "execution_snapshot_hash": self.execution_snapshot_hash,
            "files": self.files,
        }
