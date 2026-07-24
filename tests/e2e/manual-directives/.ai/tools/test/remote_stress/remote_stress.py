# ryeos:signed:2026-06-07T05:42:18Z:dcb747dfabd2a00f574629a343e7cb15e46d0388ddf444709e5473d48ca4a581:6L6vlOraNwQN3jDVpTt455BH+i7rQlyBzU/85rYz/DWpbpmE+nU7ofEb8pwtVZG1+3OldPD+rDtFLpr5VciODw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: test/remote_stress
#   version: "1.0.0"
#   tool_type: python
#   executor_id: ryeos/core/runtimes/python/function
#   tool_description: "Stress test for remote execution — CPU, stdlib, data structures"
"""Stress test tool for remote execution.

Exercises multiple capabilities: CPU work, stdlib imports, nested data
structures, error paths, and both sync/async execution signatures.
"""

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
