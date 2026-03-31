"""Per-principal encrypted secret store.

Stores sealed envelopes on disk keyed by secret name,
scoped to a principal's CAS directory.
"""

import datetime
import json
import logging
import os
import tempfile
from pathlib import Path

from rye.primitives.sealed_envelope import decrypt_and_inject, is_safe_secret_name

logger = logging.getLogger(__name__)


def _atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_path = tempfile.mkstemp(dir=path.parent)
    try:
        os.write(fd, data)
        os.fsync(fd)
        os.close(fd)
        os.replace(tmp_path, path)
    except:
        os.close(fd)
        os.unlink(tmp_path)
        raise
    dir_fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)


def vault_root(cas_base: str, user_fp: str) -> Path:
    """Return the vault directory for a principal."""
    return Path(cas_base) / user_fp / "vault"


def set_secret(cas_base: str, user_fp: str, name: str, envelope: dict, signing_key_dir: str, validate: bool = True) -> None:
    """Store a sealed envelope for a named secret. Atomic write, 0600 perms."""
    if not is_safe_secret_name(name):
        raise ValueError(f"Invalid secret name: {name!r}")

    if validate:
        decrypted = decrypt_and_inject(envelope, signing_key_dir)
        if list(decrypted.keys()) != [name] or not isinstance(decrypted[name], str):
            raise ValueError(
                f"Envelope must contain exactly {{'{name}': '<string>'}}, "
                f"got keys {sorted(decrypted.keys())}"
            )

    vdir = vault_root(cas_base, user_fp)
    vdir.mkdir(parents=True, exist_ok=True)
    vdir.chmod(0o700)

    record = {
        "schema": "vault_secret/v1",
        "name": name,
        "updated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "envelope": envelope,
    }

    secret_path = vdir / f"{name}.json"
    _atomic_write(secret_path, json.dumps(record, indent=2).encode())
    secret_path.chmod(0o600)

    logger.info("Stored vault secret %s for %s", name, user_fp)


def list_secrets(cas_base: str, user_fp: str) -> list[str]:
    """Return sorted list of secret names (no values)."""
    vdir = vault_root(cas_base, user_fp)
    if not vdir.is_dir():
        return []
    return sorted(p.stem for p in vdir.glob("*.json"))


def delete_secret(cas_base: str, user_fp: str, name: str) -> bool:
    """Delete a secret. Returns True if it existed."""
    if not is_safe_secret_name(name):
        raise ValueError(f"Invalid secret name: {name!r}")

    secret_path = vault_root(cas_base, user_fp) / f"{name}.json"
    try:
        secret_path.unlink()
    except FileNotFoundError:
        return False

    logger.info("Deleted vault secret %s for %s", name, user_fp)
    return True


def get_secret_envelope(cas_base: str, user_fp: str, name: str) -> dict | None:
    """Return the sealed envelope for a secret, or None if not found."""
    if not is_safe_secret_name(name):
        raise ValueError(f"Invalid secret name: {name!r}")

    secret_path = vault_root(cas_base, user_fp) / f"{name}.json"
    try:
        record = json.loads(secret_path.read_bytes())
    except FileNotFoundError:
        return None

    return record.get("envelope")


def resolve_vault_env(
    cas_base: str,
    user_fp: str,
    names: list[str],
    signing_key_dir: str,
) -> dict[str, str]:
    """Decrypt multiple vault secrets and return as env map.

    Raises FileNotFoundError if any named secret doesn't exist.
    Uses decrypt_and_inject from rye.primitives.sealed_envelope.
    """
    env: dict[str, str] = {}
    for name in names:
        envelope = get_secret_envelope(cas_base, user_fp, name)
        if envelope is None:
            raise FileNotFoundError(f"Vault secret not found: {name!r}")
        decrypted = decrypt_and_inject(envelope, signing_key_dir)
        env.update(decrypted)
    return env
