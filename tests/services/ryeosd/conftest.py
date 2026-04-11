"""Shared fixtures for ryeosd daemon tests.

Pattern: build the binary once (session scope), start a fresh daemon per
test class or function (function scope) in an isolated tmp_path.

Uses PROJECT_ROOT from the root conftest.py — no fragile parents[] chains.
"""

import json
import os
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from conftest import PROJECT_ROOT

# ---------------------------------------------------------------------------
# Session-scoped binary build
# ---------------------------------------------------------------------------

_RYEOSD_BINARY: Path | None = None


@pytest.fixture(scope="session", autouse=True)
def _build_ryeosd():
    """Build ryeosd once at the start of the test session."""
    global _RYEOSD_BINARY
    result = subprocess.run(
        ["cargo", "build"],
        cwd=str(PROJECT_ROOT / "ryeosd"),
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        pytest.fail(f"cargo build failed:\n{result.stderr}")
    _RYEOSD_BINARY = PROJECT_ROOT / "ryeosd" / "target" / "debug" / "ryeosd"
    assert _RYEOSD_BINARY.exists(), f"Binary not found at {_RYEOSD_BINARY}"


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_healthy(url: str, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    last_err = None
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{url}/health", timeout=2) as resp:
                data = json.loads(resp.read())
                if data.get("status") == "ok":
                    return
        except (urllib.error.URLError, ConnectionRefusedError, OSError) as e:
            last_err = e
        time.sleep(0.1)
    raise TimeoutError(f"Daemon not healthy at {url} after {timeout}s: {last_err}")


def daemon_request(daemon_info: dict, method: str, path: str, body=None):
    """Make an HTTP request to the daemon.

    Args:
        daemon_info: dict from the ``daemon`` fixture
        method: HTTP method (GET, POST, DELETE, etc.)
        path: URL path (e.g. "/health")
        body: optional dict to JSON-encode as request body

    Returns:
        (status_code: int, parsed_json: dict)
    """
    url = f"{daemon_info['url']}{path}"
    data = json.dumps(body).encode("utf-8") if body is not None else None
    req = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/json"} if data is not None else {},
        method=method,
    )
    try:
        with urllib.request.urlopen(req) as resp:
            return resp.status, json.loads(resp.read())
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        try:
            return e.code, json.loads(raw)
        except json.JSONDecodeError:
            return e.code, {"error": raw}


# ---------------------------------------------------------------------------
# Per-test daemon fixture
# ---------------------------------------------------------------------------


@pytest.fixture
def daemon(tmp_path):
    """Start an isolated ryeosd daemon for one test.

    Yields a dict:
        url:        str   — e.g. "http://127.0.0.1:PORT"
        state_dir:  Path  — daemon state directory
        cas_root:   Path  — CAS root
        process:    Popen — the daemon process (for manual inspection)
    """
    assert _RYEOSD_BINARY is not None, "ryeosd binary not built"

    port = _free_port()
    state_dir = tmp_path / "state"
    cas_root = state_dir / "cas"
    db_path = state_dir / "db" / "ryeosd.sqlite3"
    uds_path = tmp_path / "ryeosd.sock"
    url = f"http://127.0.0.1:{port}"

    proc = subprocess.Popen(
        [
            str(_RYEOSD_BINARY),
            "--bind", f"127.0.0.1:{port}",
            "--db-path", str(db_path),
            "--uds-path", str(uds_path),
            "--cas-root", str(cas_root),
            "--init-if-missing",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    try:
        _wait_healthy(url)
    except TimeoutError:
        proc.kill()
        out, err = proc.communicate(timeout=5)
        pytest.fail(
            f"Daemon did not start on port {port}:\n"
            f"--- stdout ---\n{out.decode()}\n"
            f"--- stderr ---\n{err.decode()}"
        )

    info = {
        "url": url,
        "state_dir": state_dir,
        "cas_root": cas_root,
        "process": proc,
    }

    yield info

    # Teardown: SIGTERM → wait → SIGKILL fallback
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=3)
