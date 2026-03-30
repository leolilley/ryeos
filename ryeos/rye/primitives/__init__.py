"""Rye primitives: stateless execution units."""

from rye.errors import (
    AuthenticationRequired,
    ConfigurationError,
    IntegrityError,
    LockfileError,
    RefreshError,
    ToolExecutionError,
    ValidationError,
)
from rye.runtime.http_client import HttpClientPrimitive, HttpResult, ReturnSink
from rye.primitives.cas import (
    get_blob,
    get_object,
    has,
    has_many,
    store_blob,
    store_object,
)
from rye.primitives.integrity import (
    canonical_json,
    compute_integrity,
)
from rye.runtime.lockfile import Lockfile, LockfileManager, LockfileRoot
from rye.primitives.subprocess import (
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
