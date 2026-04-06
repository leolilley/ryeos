"""Shared detached process launcher for async execution.

Consolidates spawn plumbing used by walker (core bundle) and
thread_directive (standard bundle) into a single engine-layer helper.

Uses ExecutePrimitive.spawn() (lillux) for cross-platform
detached process spawning with session isolation.

``spawn_thread()`` is the high-level lifecycle helper that handles the
full register → spawn → update PID → error-on-failure sequence.
``launch_detached()`` is the lower-level spawn primitive it wraps.
"""

import logging
import os
import time
from pathlib import Path
from typing import Dict, List, Optional

logger = logging.getLogger(__name__)


def generate_thread_id(item_id: str) -> str:
    """Generate a name-based thread ID for async/fork execution.

    Convention::

        {item_id}/{bare_name}-{epoch_ms}-{pid}-{rand}

    Includes PID and random suffix to avoid collisions when the same
    item is spawned multiple times within the same millisecond.

    Graph run IDs are managed separately by the walker.
    """
    import secrets

    epoch_ms = int(time.time() * 1000)
    bare_name = item_id.rsplit("/", 1)[-1]
    return f"{item_id}/{bare_name}-{epoch_ms}-{os.getpid()}-{secrets.token_hex(2)}"

# Env var prefixes forwarded to detached child processes.
# lillux daemonizes with a clean env — only explicitly passed vars survive.
_FORWARD_PREFIXES = (
    "PYTHON", "RYE_", "USER_SPACE", "ZEN_",
    "ANTHROPIC_", "OPENAI_", "GOOGLE_", "CONTEXT7_",
)

# Individual env vars always forwarded (system essentials).
_FORWARD_KEYS = ("HOME", "PATH", "LANG", "TERM")


def collect_env(extra: Optional[Dict[str, str]] = None) -> Dict[str, str]:
    """Build env dict for a detached child process.

    Forwards API keys, Python paths, and system essentials from the
    current environment.  Extra vars (e.g. RYE_PARENT_THREAD_ID) are
    merged on top — they take precedence over os.environ.
    """
    envs: Dict[str, str] = {}
    for key in os.environ:
        if key.startswith(_FORWARD_PREFIXES):
            envs[key] = os.environ[key]
    for key in _FORWARD_KEYS:
        if key in os.environ:
            envs[key] = os.environ[key]
    if extra:
        envs.update(extra)
    return envs


async def launch_detached(
    cmd: List[str],
    *,
    thread_id: str,
    log_dir: Path,
    env_extra: Optional[Dict[str, str]] = None,
    input_data: Optional[str] = None,
) -> Dict:
    """Spawn a detached child process via lillux.

    Args:
        cmd: Command list (e.g. [sys.executable, script, "--flag", ...]).
        thread_id: Thread identifier (for logging, not used in spawn itself).
        log_dir: Directory for spawn.log. Created if missing.
        env_extra: Additional env vars merged on top of the standard set.
        input_data: Optional string piped to child's stdin.

    Returns:
        Dict with ``success``, ``pid``, and optional ``error``.
    """
    try:
        from rye.primitives.execute import ExecutePrimitive

        log_dir.mkdir(parents=True, exist_ok=True)
        log_path = log_dir / "spawn.log"

        envs = collect_env(env_extra)

        proc = ExecutePrimitive()
        result = await proc.spawn(
            cmd=cmd[0],
            args=cmd[1:],
            log_path=str(log_path),
            envs=envs,
            input_data=input_data,
        )

        if result.success:
            logger.debug("Spawned detached process %s (pid=%s)", thread_id, result.pid)
            return {"success": True, "pid": result.pid}

        logger.error("Failed to spawn %s: %s", thread_id, result.error)
        return {"success": False, "error": result.error}
    except Exception as exc:
        logger.exception("Failed to spawn %s", thread_id)
        return {"success": False, "error": str(exc)}


async def spawn_thread(
    *,
    registry,
    thread_id: str,
    directive: str,
    cmd: List[str],
    log_dir: Path,
    input_data: Optional[str] = None,
    parent_id: Optional[str] = None,
    env_extra: Optional[Dict[str, str]] = None,
) -> Dict:
    """Register a thread, spawn a detached child, and update PID.

    Encapsulates the full lifecycle so callers can't forget a step:
      1. Register in ThreadRegistry (``created``)
      2. Mark ``running``
      3. Spawn via ``launch_detached()``
      4. On success: update PID to the child's actual PID
      5. On failure: mark ``error``

    Args:
        registry: ThreadRegistry instance (must support register,
            update_status, update_pid).
        thread_id: Thread identifier (from ``generate_thread_id()``).
        directive: Directive/item string for registry (e.g. "tool/my-tool").
        cmd: Command list for the child process.
        log_dir: Directory for spawn.log.
        input_data: Optional JSON payload piped to child stdin.
        parent_id: Optional parent thread ID.
        env_extra: Additional env vars for the child.

    Returns:
        Dict with ``success``, ``pid``, and optional ``error``.
    """
    registry.register(thread_id, directive, parent_id)
    registry.update_status(thread_id, "running")

    result = await launch_detached(
        cmd,
        thread_id=thread_id,
        log_dir=log_dir,
        input_data=input_data,
        env_extra=env_extra,
    )

    if result.get("success"):
        registry.update_pid(thread_id, result["pid"])
    else:
        registry.update_status(thread_id, "error")

    return result
