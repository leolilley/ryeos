# rye:signed:2026-04-10T00:57:18Z:97d094d2098f53d279e1f0a70976f185cb26847e52b88ee354cd623e08224843:ZTG2adSY0laUbg3OdESf7q9TQKoP0cfTief9kgQApR9qcXRvOuuLDOqaz49lblS0WlZZNsyynNW6EGtDzw3hBA:4b987fd4e40303ac
"""Node management tool — init, start, stop, authorize, and configure ryeos-node.

Actions:
  init       - Initialize a node space (signing keys, node.yaml, authorized_keys dir)
  start      - Start a local ryeos-node server
  stop       - Stop a running local ryeos-node server
  authorize  - Authorize a signing key to access the node
  status     - Check node health and identity
  configure  - Write remote.yaml entry pointing at a node
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/node"
__tool_description__ = "Manage ryeos-node lifecycle: init, start, stop, authorize, configure remote"

import json
import logging
import os
import signal
import subprocess
import shutil
import sys
import time
from pathlib import Path
from typing import Any, Dict

logger = logging.getLogger(__name__)

ACTIONS = ["init", "start", "stop", "authorize", "status", "configure"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "Node operation: init, start, stop, authorize, status, configure",
        },
        "path": {
            "type": "string",
            "description": "CAS base path for the node. Default: ~/.ai/node",
        },
        "port": {
            "type": "integer",
            "description": "Port for local node server. Default: 8321",
        },
        "name": {
            "type": "string",
            "description": "Node name for node.yaml identity. Default: auto-generated from fingerprint",
        },
        "remote": {
            "type": "string",
            "description": "Remote name for configure action. Default: 'default'",
        },
        "url": {
            "type": "string",
            "description": "Node URL for configure action. Default: http://127.0.0.1:<port>",
        },
        "fingerprint": {
            "type": "string",
            "description": "Key fingerprint to authorize. Default: current user's signing key",
        },
        "owner": {
            "type": "string",
            "description": "Owner label for authorized key. Default: 'local'",
        },
        "scopes": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Access scopes for authorized key. Default: ['*']",
        },
        "public_key": {
            "type": "string",
            "description": "Public key in ed25519:<base64> format. Required when authorizing a remote key by fingerprint.",
        },
        "node_id": {
            "type": "string",
            "description": "Node fingerprint (audience for signed requests). Auto-discovered from /public-key if reachable.",
        },
    },
    "required": ["action"],
}

_PID_FILE = "node.pid"


def execute(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    action = params.get("action")
    try:
        if action == "init":
            return _init(params, project_path)
        elif action == "start":
            return _start(params, project_path)
        elif action == "stop":
            return _stop(params, project_path)
        elif action == "authorize":
            return _authorize(params, project_path)
        elif action == "status":
            return _status(params, project_path)
        elif action == "configure":
            return _configure(params, project_path)
        else:
            return {"success": False, "error": f"Unknown action: {action}. Valid: {', '.join(ACTIONS)}"}
    except Exception as e:
        return {"success": False, "error": str(e)}


def _resolve_cas_path(params: Dict) -> Path:
    raw = params.get("path", "")
    if raw:
        return Path(raw).expanduser().resolve()
    ai_dir = os.environ.get("AI_DIR", ".ai")
    return Path.home() / ai_dir / "node"


def _pid_file(cas_path: Path) -> Path:
    return cas_path / _PID_FILE


# ---------------------------------------------------------------------------
# init
# ---------------------------------------------------------------------------

def _init(params: Dict, project_path: str) -> Dict:
    cas_path = _resolve_cas_path(params)
    signing_dir = cas_path / "signing"
    config_root = cas_path / "config"
    authorized_keys_dir = config_root / "authorized_keys"
    ai_dir = os.environ.get("AI_DIR", ".ai")
    node_yaml_dir = config_root / ai_dir / "config" / "node"
    node_yaml_path = node_yaml_dir / "node.yaml"

    signing_dir.mkdir(parents=True, exist_ok=True)

    from rye.primitives.signing import compute_key_fingerprint, ensure_full_keypair

    _, pub, _, _ = ensure_full_keypair(signing_dir)
    fingerprint = compute_key_fingerprint(pub)

    authorized_keys_dir.mkdir(parents=True, exist_ok=True)

    node_name = params.get("name") or f"node-{fingerprint[:8]}"
    created_yaml = False
    if not node_yaml_path.exists():
        node_yaml_dir.mkdir(parents=True, exist_ok=True)
        node_yaml_path.write_text(
            f"identity:\n"
            f"  name: {node_name}\n"
            f"  signing_key_dir: {signing_dir}\n"
            f"hardware:\n"
            f"  gpus: 0\n"
            f"  memory_gb: 2\n"
            f"features:\n"
            f"  registry: false\n"
            f"  webhooks: true\n"
            f"limits:\n"
            f"  max_concurrent: 8\n"
            f"coordination:\n"
            f"  type: asyncio\n",
            encoding="utf-8",
        )
        created_yaml = True

    return {
        "success": True,
        "cas_path": str(cas_path),
        "fingerprint": f"fp:{fingerprint}",
        "node_name": node_name,
        "signing_key_dir": str(signing_dir),
        "authorized_keys_dir": str(authorized_keys_dir),
        "node_yaml": str(node_yaml_path),
        "created_yaml": created_yaml,
        "next_steps": [
            f"Authorize your key: rye execute tool rye/core/node/node with {{\"action\": \"authorize\", \"path\": \"{cas_path}\"}}",
            f"Start the node: rye execute tool rye/core/node/node with {{\"action\": \"start\", \"path\": \"{cas_path}\"}}",
        ],
    }


# ---------------------------------------------------------------------------
# authorize
# ---------------------------------------------------------------------------

def _authorize(params: Dict, project_path: str) -> Dict:
    cas_path = _resolve_cas_path(params)
    config_root = cas_path / "config"
    authorized_keys_dir = config_root / "authorized_keys"
    signing_dir = cas_path / "signing"

    if not signing_dir.exists():
        return {"success": False, "error": f"Node not initialized at {cas_path}. Run init first."}

    import base64
    from rye.primitives.signing import (
        compute_key_fingerprint,
        ensure_keypair,
        load_keypair,
        sign_hash,
    )

    fp = params.get("fingerprint", "")
    owner = params.get("owner", "local")
    scopes = params.get("scopes", ["*"])

    if not fp:
        # Default: authorize the current user's signing key
        from rye.utils.path_utils import get_signing_key_dir
        user_key_dir = get_signing_key_dir()
        _, user_pub = ensure_keypair(user_key_dir)
        fp = compute_key_fingerprint(user_pub)
        pub_b64 = base64.b64encode(user_pub).decode()
    else:
        # Authorizing a remote key — need its public key
        pub_key_param = params.get("public_key", "")
        if not pub_key_param:
            return {"success": False, "error": "Cannot authorize a remote key without its public key. Provide public_key parameter in ed25519:<base64> format."}
        # Accept with or without ed25519: prefix
        if pub_key_param.startswith("ed25519:"):
            pub_b64 = pub_key_param[len("ed25519:"):]
        else:
            pub_b64 = pub_key_param

    # Strip fp: prefix if present
    if fp.startswith("fp:"):
        fp = fp[3:]

    # Build the authorized key TOML body
    scopes_toml = ", ".join(f'"{s}"' for s in scopes)
    body = (
        f'fingerprint = "{fp}"\n'
        f'owner = "{owner}"\n'
        f'public_key = "ed25519:{pub_b64}"\n'
        f'scopes = [{scopes_toml}]\n'
    )

    # Sign with the node's key
    import hashlib
    node_priv, node_pub = load_keypair(signing_dir)
    node_fp = compute_key_fingerprint(node_pub)
    content_hash = hashlib.sha256(body.encode()).hexdigest()
    sig_b64 = sign_hash(content_hash, node_priv)
    timestamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

    signed_content = f"# rye:signed:{timestamp}:{content_hash}:{sig_b64}:{node_fp}\n{body}"

    authorized_keys_dir.mkdir(parents=True, exist_ok=True)
    key_file = authorized_keys_dir / f"{fp}.toml"
    key_file.write_text(signed_content, encoding="utf-8")

    return {
        "success": True,
        "fingerprint": f"fp:{fp}",
        "owner": owner,
        "scopes": scopes,
        "key_file": str(key_file),
        "signed_by": f"fp:{node_fp}",
    }


# ---------------------------------------------------------------------------
# start
# ---------------------------------------------------------------------------

def _start(params: Dict, project_path: str) -> Dict:
    cas_path = _resolve_cas_path(params)
    port = params.get("port", 8321)
    pid_path = _pid_file(cas_path)

    if not (cas_path / "signing").exists():
        return {"success": False, "error": f"Node not initialized at {cas_path}. Run init first."}

    # Check if already running
    if pid_path.exists():
        try:
            pid = int(pid_path.read_text().strip())
            os.kill(pid, 0)  # Check if process exists
            return {
                "success": True,
                "already_running": True,
                "pid": pid,
                "port": port,
                "url": f"http://127.0.0.1:{port}",
            }
        except (ProcessLookupError, ValueError):
            pid_path.unlink(missing_ok=True)

    # Find python — use the same interpreter that's running rye
    python_bin = sys.executable

    # Verify uvicorn is importable
    check = subprocess.run(
        [python_bin, "-c", "import uvicorn"],
        capture_output=True, text=True,
    )
    if check.returncode != 0:
        return {"success": False, "error": "uvicorn not importable. Install: pip install uvicorn[standard]"}

    # Start server as background process
    log_file = cas_path / "node.log"
    env = os.environ.copy()
    env["CAS_BASE_PATH"] = str(cas_path)

    with open(log_file, "a") as lf:
        proc = subprocess.Popen(
            [
                python_bin, "-m", "uvicorn",
                "ryeos_node.server:app",
                "--host", "127.0.0.1",
                "--port", str(port),
            ],
            env=env,
            stdout=lf,
            stderr=lf,
            start_new_session=True,
        )

    pid_path.write_text(str(proc.pid), encoding="utf-8")

    # Wait briefly and check health
    time.sleep(2)
    healthy = False
    try:
        import urllib.request
        resp = urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=5)
        if resp.status == 200:
            healthy = True
    except Exception:
        pass

    return {
        "success": True,
        "pid": proc.pid,
        "port": port,
        "url": f"http://127.0.0.1:{port}",
        "log_file": str(log_file),
        "healthy": healthy,
    }


# ---------------------------------------------------------------------------
# stop
# ---------------------------------------------------------------------------

def _stop(params: Dict, project_path: str) -> Dict:
    cas_path = _resolve_cas_path(params)
    pid_path = _pid_file(cas_path)

    if not pid_path.exists():
        return {"success": True, "message": "No node running (no pid file)"}

    try:
        pid = int(pid_path.read_text().strip())
    except (ValueError, FileNotFoundError):
        pid_path.unlink(missing_ok=True)
        return {"success": True, "message": "Stale pid file removed"}

    try:
        os.kill(pid, signal.SIGTERM)
        # Wait for graceful shutdown
        for _ in range(10):
            time.sleep(0.5)
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
        else:
            os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass

    pid_path.unlink(missing_ok=True)
    return {"success": True, "stopped_pid": pid}


# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------

def _status(params: Dict, project_path: str) -> Dict:
    cas_path = _resolve_cas_path(params)
    port = params.get("port", 8321)
    url = params.get("url", f"http://127.0.0.1:{port}")

    # Check local pid
    pid_path = _pid_file(cas_path)
    local_pid = None
    if pid_path.exists():
        try:
            pid = int(pid_path.read_text().strip())
            os.kill(pid, 0)
            local_pid = pid
        except (ProcessLookupError, ValueError):
            pass

    # Check health endpoint
    health = None
    node_status = None
    try:
        import urllib.request
        resp = urllib.request.urlopen(f"{url}/health", timeout=5)
        health = json.loads(resp.read())
    except Exception:
        pass

    try:
        import urllib.request
        resp = urllib.request.urlopen(f"{url}/status", timeout=5)
        node_status = json.loads(resp.read())
    except Exception:
        pass

    return {
        "success": True,
        "cas_path": str(cas_path),
        "local_pid": local_pid,
        "url": url,
        "healthy": health is not None,
        "health": health,
        "node_status": node_status,
    }


# ---------------------------------------------------------------------------
# configure
# ---------------------------------------------------------------------------

def _configure(params: Dict, project_path: str) -> Dict:
    """Write a remote.yaml entry pointing at a node."""
    import urllib.request
    import yaml

    port = params.get("port", 8321)
    remote_name = params.get("remote", "default")
    url = params.get("url", f"http://127.0.0.1:{port}")

    # Auto-discover node_id from the node's /public-key endpoint
    node_id = ""
    try:
        resp = urllib.request.urlopen(f"{url}/public-key", timeout=5)
        identity = json.loads(resp.read())
        node_id = identity.get("principal_id", "")
    except Exception:
        node_id = params.get("node_id", "")

    proj = Path(project_path)
    ai_dir = os.environ.get("AI_DIR", ".ai")
    config_dir = proj / ai_dir / "config" / "cas"
    config_dir.mkdir(parents=True, exist_ok=True)
    remote_yaml = config_dir / "remote.yaml"

    # Load existing or start fresh
    existing = {}
    if remote_yaml.exists():
        existing = yaml.safe_load(remote_yaml.read_text()) or {}

    entry = {"url": url}
    if node_id:
        entry["node_id"] = node_id

    remotes = existing.get("remotes", {})
    remotes[remote_name] = entry
    existing["remotes"] = remotes

    remote_yaml.write_text(
        yaml.dump(existing, default_flow_style=False, sort_keys=False),
        encoding="utf-8",
    )

    result = {
        "success": True,
        "remote_name": remote_name,
        "url": url,
        "config_file": str(remote_yaml),
    }
    if node_id:
        result["node_id"] = node_id
    else:
        result["warning"] = "Could not discover node_id. Provide node_id parameter or ensure the node is reachable."
    return result
