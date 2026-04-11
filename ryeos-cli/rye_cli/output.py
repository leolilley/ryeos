"""Shared output utilities for CLI verbs."""

import asyncio
import http.client
import json
import os
import socket
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable, Coroutine, Dict


def run_async(coro: Coroutine) -> Any:
    """Run an async coroutine from sync context."""
    return asyncio.run(coro)


def _default_uds_path() -> str | None:
    """Resolve the default UDS path using the same logic as ryeosd config.rs."""
    runtime_dir = os.environ.get("XDG_RUNTIME_DIR")
    if runtime_dir:
        return str(Path(runtime_dir) / "ryeosd.sock")
    uid = os.geteuid() if hasattr(os, "geteuid") else 0
    return str(Path(tempfile.gettempdir()) / f"ryeosd-{uid}" / "ryeosd.sock")


def daemon_url() -> str:
    """Get the daemon base URL from env or default."""
    return os.environ.get("RYEOSD_URL", "http://127.0.0.1:7400")


def _uds_path() -> str | None:
    """Get the UDS socket path, or None to fall back to TCP."""
    explicit = os.environ.get("RYEOSD_SOCKET_PATH")
    if explicit:
        return explicit
    # If RYEOSD_URL is explicitly set, honour TCP and skip UDS probing.
    if os.environ.get("RYEOSD_URL"):
        return None
    path = _default_uds_path()
    if path and os.path.exists(path):
        return path
    return None


class _UnixConnection(http.client.HTTPConnection):
    """HTTPConnection subclass that connects over a Unix domain socket."""

    def __init__(self, uds_path: str, timeout: float = 30):
        super().__init__("localhost", timeout=timeout)
        self._uds_path = uds_path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self._uds_path)


def _daemon_request(method: str, path: str, body: bytes | None = None,
                    headers: dict | None = None) -> http.client.HTTPResponse:
    """Send an HTTP request to the daemon, preferring UDS over TCP."""
    uds = _uds_path()
    if uds:
        conn = _UnixConnection(uds)
    else:
        url = daemon_url()
        # Parse host:port from URL
        host_port = url.replace("http://", "").replace("https://", "")
        conn = http.client.HTTPConnection(host_port)

    conn.request(method, path, body=body, headers=headers or {})
    return conn.getresponse()


def daemon_execute(item_ref: str, parameters: dict = None, launch_mode: str = "inline",
                   model: str = None, budget: dict = None) -> dict:
    """Submit an execution request to the ryeosd daemon."""
    payload = {
        "item_ref": item_ref,
        "parameters": parameters or {},
        "launch_mode": launch_mode,
    }
    if model:
        payload["model"] = model
    if budget:
        payload["budget"] = budget

    data = json.dumps(payload).encode("utf-8")
    try:
        resp = _daemon_request(
            "POST", "/execute", body=data,
            headers={"Content-Type": "application/json"},
        )
        body = resp.read().decode("utf-8", errors="replace")
        if resp.status >= 400:
            try:
                err = json.loads(body)
            except json.JSONDecodeError:
                err = {"error": body}
            return {"status": "error", "error": err.get("error", body)}
        return json.loads(body)
    except (OSError, ConnectionError) as e:
        target = _uds_path() or daemon_url()
        return {"status": "error", "error": f"Cannot connect to daemon at {target}: {e}"}


def print_result(result: Dict, compact: bool = False) -> None:
    """Print a result dict as JSON to stdout. Exit 1 on error status."""
    indent = None if compact else 2
    print(json.dumps(result, indent=indent, default=str))
    if result.get("status") == "error" or result.get("error"):
        sys.exit(1)


def die(msg: str, code: int = 1) -> None:
    """Print error to stderr and exit."""
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(code)


def parse_params(raw: str) -> Dict:
    """Parse a JSON params string, exiting on invalid JSON."""
    try:
        params = json.loads(raw)
    except json.JSONDecodeError as e:
        die(f"invalid JSON in params: {e}")
    if not isinstance(params, dict):
        die("params must be a JSON object")
    return params
