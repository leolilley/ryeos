"""Rye primitives: stateless execution units."""

from rye.errors import (
    AuthenticationRequired,
    ConfigurationError,
    IntegrityError,
    RefreshError,
    ToolExecutionError,
    ValidationError,
)
from rye.primitives.cas import (
    get_blob,
    get_object,
    has,
    has_many,
    store_blob,
    store_object,
)
from rye.primitives.execute import (
    ExecutePrimitive,
    ExecuteResult,
    SpawnResult,
    KillResult,
    StatusResult,
)
from rye.primitives.integrity import (
    canonical_json,
    compute_integrity,
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
    "ConfigurationError",
    "AuthenticationRequired",
    "RefreshError",
    # Integrity
    "canonical_json",
    "compute_integrity",
    # Execute
    "ExecuteResult",
    "ExecutePrimitive",
    "SpawnResult",
    "KillResult",
    "StatusResult",
]
