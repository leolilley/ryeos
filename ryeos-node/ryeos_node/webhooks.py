"""Local filesystem webhook binding management.

Replaces Supabase webhook_bindings table with a JSON index
file and per-hook HMAC secret files.
"""

import datetime
import json
import logging
import os
import secrets
from pathlib import Path

logger = logging.getLogger(__name__)


def _atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    tmp.write_bytes(data)
    os.replace(tmp, path)
    fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _read_index(cas_base: str) -> dict:
    index_path = Path(cas_base) / "webhooks" / "bindings.json"
    if index_path.exists():
        return json.loads(index_path.read_bytes())
    return {}


def _write_index(cas_base: str, index: dict) -> None:
    index_path = Path(cas_base) / "webhooks" / "bindings.json"
    _atomic_write(index_path, json.dumps(index, indent=2).encode())


def create_binding(
    cas_base: str,
    user_fp: str,
    remote_name: str,
    item_id: str,
    project_path: str,
    description: str | None = None,
    secret_envelope: dict | None = None,
    owner: str = "",
    vault_keys: list[str] | None = None,
) -> dict:
    hook_id = f"wh_{secrets.token_hex(16)}"
    hmac_secret = f"whsec_{secrets.token_hex(32)}"

    record = {
        "hook_id": hook_id,
        "user_id": user_fp,
        "remote_name": remote_name,
        "item_id": item_id,
        "project_path": project_path,
        "description": description,
        "secret_envelope": secret_envelope,
        "vault_keys": vault_keys or [],
        "owner": owner,
        "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "revoked_at": None,
        "active": True,
    }

    index = _read_index(cas_base)
    index[hook_id] = record
    _write_index(cas_base, index)

    secret_path = Path(cas_base) / "webhooks" / "secrets" / f"{hook_id}.key"
    secret_path.parent.mkdir(parents=True, exist_ok=True)
    secret_path.write_text(hmac_secret)
    secret_path.chmod(0o600)

    logger.info("Created webhook binding %s for %s", hook_id, item_id)

    return {
        "hook_id": hook_id,
        "hmac_secret": hmac_secret,
        "item_id": item_id,
        "project_path": project_path,
        "has_secret_envelope": secret_envelope is not None,
        "vault_keys": vault_keys or [],
    }


def list_bindings(cas_base: str, user_fp: str, remote_name: str) -> list[dict]:
    index = _read_index(cas_base)
    results = []
    for binding in index.values():
        if binding["user_id"] == user_fp and binding["remote_name"] == remote_name:
            results.append({
                "hook_id": binding["hook_id"],
                "item_id": binding["item_id"],
                "project_path": binding["project_path"],
                "description": binding["description"],
                "created_at": binding["created_at"],
                "revoked_at": binding["revoked_at"],
                "has_secret_envelope": binding.get("secret_envelope") is not None,
                "vault_keys": binding.get("vault_keys", []),
                "owner": binding.get("owner", ""),
            })
    results.sort(key=lambda b: b["created_at"], reverse=True)
    return results


def revoke_binding(
    cas_base: str, hook_id: str, user_fp: str, remote_name: str
) -> bool:
    index = _read_index(cas_base)
    binding = index.get(hook_id)
    if not binding:
        return False
    if binding["user_id"] != user_fp or binding["remote_name"] != remote_name:
        return False
    if binding["revoked_at"] is not None:
        return False

    binding["revoked_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    binding["active"] = False
    _write_index(cas_base, index)

    secret_path = Path(cas_base) / "webhooks" / "secrets" / f"{hook_id}.key"
    try:
        secret_path.unlink()
    except FileNotFoundError:
        pass

    logger.info("Revoked webhook binding %s", hook_id)
    return True


def resolve_binding(
    cas_base: str, hook_id: str, remote_name: str
) -> dict | None:
    index = _read_index(cas_base)
    binding = index.get(hook_id)
    if not binding:
        return None
    if binding["remote_name"] != remote_name:
        return None
    if binding["revoked_at"] is not None:
        return None

    secret_path = Path(cas_base) / "webhooks" / "secrets" / f"{hook_id}.key"
    try:
        hmac_secret = secret_path.read_text()
    except FileNotFoundError:
        logger.warning("Secret file missing for active binding %s", hook_id)
        return None

    return {**binding, "hmac_secret": hmac_secret}
