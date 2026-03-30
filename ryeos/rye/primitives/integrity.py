"""Integrity hashing primitives.

Provides deterministic SHA256 hashing for arbitrary data.
Uses canonical JSON serialization (sorted keys, no whitespace) to ensure
the same input always produces the same hash.

Lillux is type-agnostic — it doesn't know about tools, directives, or knowledge.
Callers (e.g., rye) are responsible for structuring the data dict appropriately
for their item types.
"""

import hashlib
import json
from typing import Any, Dict


def canonical_json(data: Any) -> str:
    """Serialize data to canonical JSON.

    Canonical form: sorted keys, no whitespace, consistent formatting.

    Args:
        data: Data to serialize.

    Returns:
        Canonical JSON string.
    """
    return json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def compute_integrity(data: Dict[str, Any]) -> str:
    """Compute deterministic SHA256 hash for arbitrary data.

    The caller is responsible for constructing the data dict with
    whatever fields are relevant to their use case. This function
    simply canonicalizes and hashes.

    Args:
        data: Dict to hash. Must be JSON-serializable.

    Returns:
        SHA256 hex digest (64 chars).
    """
    canonical = canonical_json(data)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()
