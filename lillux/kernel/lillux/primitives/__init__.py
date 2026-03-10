"""Lillux primitives: stateless execution units."""

from lillux.primitives.errors import (
    AuthenticationRequired,
    ConfigurationError,
    IntegrityError,
    LockfileError,
    RefreshError,
    ToolExecutionError,
    ValidationError,
)
from lillux.primitives.http_client import HttpClientPrimitive, HttpResult, ReturnSink
from lillux.primitives.cas import (
    get_blob,
    get_object,
    has,
    has_many,
    store_blob,
    store_object,
)
from lillux.primitives.integrity import (
    canonical_json,
    compute_integrity,
)
from lillux.primitives.lockfile import Lockfile, LockfileManager, LockfileRoot
from lillux.primitives.subprocess import (
    SubprocessPrimitive,
    SubprocessResult,
    SpawnResult,
    KillResult,
    StatusResult,
)

__all__ = [
    # CAS
    "store_blob",
    "store_object",
    "get_blob",
    "get_object",
    "has",
    "has_many",
    # Errors
    "ValidationError",
    "ToolExecutionError",
    "IntegrityError",
    "LockfileError",
    "ConfigurationError",
    "AuthenticationRequired",
    "RefreshError",
    # Integrity
    "canonical_json",
    "compute_integrity",
    # Lockfile
    "LockfileRoot",
    "Lockfile",
    "LockfileManager",
    # Subprocess
    "SubprocessResult",
    "SubprocessPrimitive",
    "SpawnResult",
    "KillResult",
    "StatusResult",
    # HTTP Client
    "HttpResult",
    "HttpClientPrimitive",
    "ReturnSink",
]
