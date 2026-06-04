# ryeos:signed:2026-04-09T00:59:52Z:edd79b16d12bcf0e6d31df19f2f8f44c3af161a8608e19f69208138b9c6767a4:9tUE3rfkoq4sI9x7oJWtjhRetho488CA69IeIs1u-yZ4sK1bcpUhaBEWvkOg_zYlNuUzVdsIOBKhi5Ra9RWoDQ:4b987fd4e40303ac
"""Stress test tool for remote execution.

Exercises multiple capabilities: CPU work, stdlib imports, nested data
structures, error paths, and both sync/async execution signatures.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "ryeos/core/runtimes/python/function"
__category__ = "test/remote_stress"
__tool_description__ = "Stress test for remote execution — CPU, stdlib, data structures"

import hashlib
import json
import math
import os
import platform
import time

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ["compute", "env_info", "data_transform", "error"],
            "description": "Which sub-test to run",
        },
        "iterations": {
            "type": "integer",
            "default": 1000,
            "description": "Iteration count for compute action",
        },
    },
    "required": ["action"],
}


def _compute(iterations: int) -> dict:
    """CPU-bound work: hash chain + math."""
    start = time.monotonic()
    digest = b"seed"
    for _ in range(iterations):
        digest = hashlib.sha256(digest).digest()
    primes = []
    for n in range(2, min(iterations, 500)):
        if all(n % i != 0 for i in range(2, int(math.sqrt(n)) + 1)):
            primes.append(n)
    elapsed = time.monotonic() - start
    return {
        "final_hash": digest.hex()[:16],
        "prime_count": len(primes),
        "largest_prime": primes[-1] if primes else None,
        "elapsed_ms": round(elapsed * 1000, 2),
    }


def _env_info() -> dict:
    """Gather execution environment details."""
    return {
        "python_version": platform.python_version(),
        "platform": platform.platform(),
        "arch": platform.machine(),
        "pid": os.getpid(),
        "cwd": os.getcwd(),
        "rye_remote_name": os.environ.get("RYE_REMOTE_NAME", "(not set)"),
    }


def _data_transform() -> dict:
    """Build and transform nested data structures."""
    records = [
        {"id": i, "value": math.sin(i) * 100, "tag": f"item-{i:04d}"}
        for i in range(50)
    ]
    total = sum(r["value"] for r in records)
    by_sign = {
        "positive": [r for r in records if r["value"] >= 0],
        "negative": [r for r in records if r["value"] < 0],
    }
    serialized = json.dumps(records)
    return {
        "record_count": len(records),
        "total": round(total, 4),
        "positive_count": len(by_sign["positive"]),
        "negative_count": len(by_sign["negative"]),
        "serialized_bytes": len(serialized),
        "round_trip_ok": json.loads(serialized) == records,
    }


def execute(params: dict, project_path: str) -> dict:
    """Execute the stress test tool."""
    action = params.get("action", "compute")

    if action == "compute":
        iterations = params.get("iterations", 1000)
        result = _compute(iterations)
    elif action == "env_info":
        result = _env_info()
    elif action == "data_transform":
        result = _data_transform()
    elif action == "error":
        raise RuntimeError("Intentional error for testing error propagation")
    else:
        return {"success": False, "error": f"Unknown action: {action}"}

    return {"success": True, "action": action, **result}
