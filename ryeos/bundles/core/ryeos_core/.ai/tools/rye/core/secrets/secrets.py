# rye:signed:2026-04-19T09:49:53Z:9dad6ac39d119521c86187702f914bd0a2b095f7a1b6cec82b3c07dd9f68f074:8CPHduH1DunwfD14PchNEiEK5W80bhsKUeCWlfnU8+o5OAIEY+CbcB8HkO6bnBIX36T4UxaHw4vsfnTUJ1k+BQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""
Local encrypted secret store management.

Actions:
  set    - Store a secret in the local encrypted store.
  list   - List secret names (never values).
  delete - Remove a secret from the local store.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/secrets"
__tool_description__ = "Manage local encrypted secret store"

import json
import logging
import os
from pathlib import Path
from typing import Dict

from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.hashes import SHA256
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

from rye.primitives.sealed_envelope import is_safe_secret_name
from rye.primitives.signing import load_keypair

logger = logging.getLogger(__name__)

TOOL_METADATA = {
    "name": "secrets",
    "description": "Manage local encrypted secret store",
    "version": __version__,
    "protected": True,
}

ACTIONS = ["set", "list", "delete"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "Secret store operation: set, list, delete",
        },
        "name": {
            "type": "string",
            "description": "Secret name (for set/delete)",
        },
        "value": {
            "type": "string",
            "description": "Secret value (for set)",
        },
    },
    "required": ["action"],
}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _get_key_dir() -> Path:
    """Return the signing key directory from env or default."""
    from rye.utils.path_utils import get_signing_key_dir
    return get_signing_key_dir()


def _derive_store_key(key_dir: Path) -> bytes:
    """Derive a 32-byte symmetric key from the Ed25519 private key via HKDF."""
    private_key_pem, _ = load_keypair(key_dir)
    hkdf = HKDF(
        algorithm=SHA256(),
        length=32,
        salt=None,
        info=b"ryeos-secret-store-v1",
    )
    return hkdf.derive(private_key_pem)


def _store_path() -> Path:
    """Return the path to the encrypted secret store file."""
    return Path.home() / ".ai" / "secrets" / "store.enc"


def _load_store(key_dir: Path) -> dict:
    """Decrypt and load the secret store. Returns {} if not exists."""
    path = _store_path()
    if not path.exists():
        return {}

    raw = path.read_bytes()
    if len(raw) < 12:
        return {}

    nonce = raw[:12]
    ciphertext = raw[12:]

    symmetric_key = _derive_store_key(key_dir)
    aead = ChaCha20Poly1305(symmetric_key)
    plaintext = aead.decrypt(nonce, ciphertext, None)
    return json.loads(plaintext)


def _save_store(store: dict, key_dir: Path) -> None:
    """Encrypt and atomically write the secret store."""
    path = _store_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    os.chmod(path.parent, 0o700)

    symmetric_key = _derive_store_key(key_dir)
    aead = ChaCha20Poly1305(symmetric_key)
    nonce = os.urandom(12)
    plaintext = json.dumps(store, sort_keys=True, separators=(",", ":")).encode()
    ciphertext = aead.encrypt(nonce, plaintext, None)

    import tempfile
    fd, tmp_path_str = tempfile.mkstemp(dir=path.parent)
    try:
        os.write(fd, nonce + ciphertext)
        os.fchmod(fd, 0o600)
        os.fsync(fd)
        os.close(fd)
        os.replace(tmp_path_str, path)
    except BaseException:
        os.close(fd)
        try:
            os.unlink(tmp_path_str)
        except OSError:
            pass
        raise


# ---------------------------------------------------------------------------
# Actions
# ---------------------------------------------------------------------------


async def _set(project_path: Path, params: Dict) -> Dict:
    """Store a secret in the local encrypted store."""
    name = params.get("name")
    value = params.get("value")

    if not name:
        return {"error": "name is required for set"}
    if not value:
        return {"error": "value is required for set"}
    if not is_safe_secret_name(name):
        return {"error": f"Invalid secret name: {name!r}. Use UPPER_SNAKE_CASE (letters, digits, underscores)."}

    key_dir = _get_key_dir()
    store = _load_store(key_dir)
    store[name] = value
    _save_store(store, key_dir)

    return {"stored": name, "message": f"Secret '{name}' stored locally"}


async def _list(project_path: Path, params: Dict) -> Dict:
    """List secret names (never values)."""
    key_dir = _get_key_dir()
    store = _load_store(key_dir)
    names = sorted(store.keys())
    return {"secrets": names, "count": len(names)}


async def _delete(project_path: Path, params: Dict) -> Dict:
    """Remove a secret from the local store."""
    name = params.get("name")
    if not name:
        return {"error": "name is required for delete"}

    key_dir = _get_key_dir()
    store = _load_store(key_dir)

    if name not in store:
        return {"error": f"Secret '{name}' not found in local store"}

    del store[name]
    _save_store(store, key_dir)

    return {"deleted": name, "message": f"Secret '{name}' removed from local store"}


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

_ACTION_MAP = {
    "set": _set,
    "list": _list,
    "delete": _delete,
}


async def execute(params: dict, project_path: str) -> dict:
    """Entry point for function runtime."""
    action = params.pop("action", None)
    if not action:
        return {"success": False, "error": "action required in params"}
    if action not in ACTIONS:
        return {"success": False, "error": f"Unknown action: {action}", "valid_actions": ACTIONS}

    pp = Path(project_path).resolve()
    if not pp.is_dir():
        return {"success": False, "error": f"Project path does not exist: {project_path}"}

    result = await _ACTION_MAP[action](pp, params)
    if "error" in result:
        result["success"] = False
    elif "success" not in result:
        result["success"] = True
    return result
