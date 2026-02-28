"""Shared output utilities for CLI verbs."""

import asyncio
import json
import sys
from typing import Any, Callable, Coroutine, Dict


def run_async(coro: Coroutine) -> Any:
    """Run an async coroutine from sync context."""
    return asyncio.run(coro)


def print_result(result: Dict, compact: bool = False) -> None:
    """Print a result dict as JSON to stdout."""
    indent = None if compact else 2
    print(json.dumps(result, indent=indent, default=str))


def die(msg: str, code: int = 1) -> None:
    """Print error to stderr and exit."""
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(code)


def parse_params(raw: str) -> Dict:
    """Parse a JSON params string, exiting on invalid JSON."""
    try:
        params = json.loads(raw)
    except json.JSONDecodeError as e:
        die(f"invalid JSON in --params: {e}")
    if not isinstance(params, dict):
        die("--params must be a JSON object")
    return params
