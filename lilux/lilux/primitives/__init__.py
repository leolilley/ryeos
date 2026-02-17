"""Lilux primitives: stateless execution units."""

from lilux.primitives.errors import (
    AuthenticationRequired,
    ConfigurationError,
    IntegrityError,
    LockfileError,
    RefreshError,
    ToolExecutionError,
    ValidationError,
)
from lilux.primitives.http_client import HttpClientPrimitive, HttpResult, ReturnSink
from lilux.primitives.integrity import (
    canonical_json,
    compute_integrity,
)
from lilux.primitives.lockfile import Lockfile, LockfileManager, LockfileRoot
from lilux.primitives.subprocess import SubprocessPrimitive, SubprocessResult

__all__ = [
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
    # HTTP Client
    "HttpResult",
    "HttpClientPrimitive",
    "ReturnSink",
]
