# rye:signed:2026-02-26T05:02:30Z:303bfcdbb721271f698837452ffd8ddb091c7ca6dab24ba7af06ec60570f5353:r6W0Gvbc7lHv4AHTVppUkU1B_Z5l0fRkVgJJdF9U7icvbEPS6cGKM3dGRhI2ITikyjg5OeknRsdtTAmjeEExCQ==:4b987fd4e40303ac
"""Key management tool — generate, inspect, and trust Ed25519 signing keys.

The user's signing identity. Handles keypair generation, fingerprint display,
and trusted key provisioning into project or user space.

Actions:
  generate - Create a new Ed25519 keypair (or return existing)
  info     - Show current key fingerprint and public key
  trust    - Add the current key to a space's trusted_keys (signed TOML)
  list     - List all trusted keys across all spaces
  remove   - Remove a key from the user trust store
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/keys"
__tool_description__ = "Manage Ed25519 signing keys and trusted key store"

from pathlib import Path
from typing import Any, Dict

from rye.constants import AI_DIR

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ["generate", "info", "trust", "list", "remove"],
            "description": (
                "Key operation: generate (create keypair), info (show fingerprint), "
                "trust (add key to space), list (show all trusted keys), "
                "remove (remove from user trust store)"
            ),
        },
        "space": {
            "type": "string",
            "enum": ["user", "project"],
            "description": "Target space for trust action. Default: user.",
        },
        "owner": {
            "type": "string",
            "description": "Owner name for the trusted key identity. Default: local.",
        },
        "fingerprint": {
            "type": "string",
            "description": "Key fingerprint for remove action.",
        },
        "force": {
            "type": "boolean",
            "description": "Force regeneration of keypair even if one exists.",
        },
    },
    "required": ["action"],
}


def execute(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Execute key management action."""
    action = params.get("action")

    try:
        if action == "generate":
            return _generate(params, project_path)
        elif action == "info":
            return _info(params, project_path)
        elif action == "trust":
            return _trust(params, project_path)
        elif action == "list":
            return _list(params, project_path)
        elif action == "remove":
            return _remove(params, project_path)
        else:
            return {
                "success": False,
                "error": f"Unknown action: {action}. Valid: generate, info, trust, list, remove",
            }
    except Exception as e:
        return {"success": False, "error": str(e)}


def _generate(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Generate an Ed25519 keypair (or return existing)."""
    from lillux.primitives.signing import ensure_keypair, compute_key_fingerprint
    from rye.utils.path_utils import get_user_space

    key_dir = get_user_space() / AI_DIR / "keys"
    force = params.get("force", False)

    if force and key_dir.exists():
        priv = key_dir / "private_key.pem"
        pub = key_dir / "public_key.pem"
        if priv.exists():
            priv.unlink()
        if pub.exists():
            pub.unlink()

    already_existed = (key_dir / "private_key.pem").exists()
    private_pem, public_pem = ensure_keypair(key_dir)
    fingerprint = compute_key_fingerprint(public_pem)

    return {
        "success": True,
        "fingerprint": fingerprint,
        "public_key_pem": public_pem.decode("utf-8").strip(),
        "key_dir": str(key_dir),
        "created": not already_existed,
        "message": (
            f"Keypair already exists (fingerprint: {fingerprint})"
            if already_existed
            else f"Generated new Ed25519 keypair (fingerprint: {fingerprint})"
        ),
    }


def _info(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Show current key fingerprint and public key."""
    from lillux.primitives.signing import ensure_keypair, compute_key_fingerprint
    from rye.utils.path_utils import get_user_space

    key_dir = get_user_space() / AI_DIR / "keys"

    if not (key_dir / "private_key.pem").exists():
        return {
            "success": False,
            "error": "No keypair found. Run action 'generate' first.",
            "key_dir": str(key_dir),
        }

    _, public_pem = ensure_keypair(key_dir)
    fingerprint = compute_key_fingerprint(public_pem)

    # Check trust status across spaces
    from rye.utils.trust_store import TrustStore

    store = TrustStore(project_path=Path(project_path))
    key_info = store.get_key(fingerprint)

    return {
        "success": True,
        "fingerprint": fingerprint,
        "public_key_pem": public_pem.decode("utf-8").strip(),
        "key_dir": str(key_dir),
        "trusted": key_info is not None,
        "trusted_source": key_info.source if key_info else None,
    }


def _trust(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Add the current signing key to a space's trusted_keys."""
    from lillux.primitives.signing import ensure_keypair, compute_key_fingerprint
    from rye.utils.path_utils import get_user_space
    from rye.utils.trust_store import TrustStore

    key_dir = get_user_space() / AI_DIR / "keys"

    if not (key_dir / "private_key.pem").exists():
        return {
            "success": False,
            "error": "No keypair found. Run action 'generate' first.",
        }

    _, public_pem = ensure_keypair(key_dir)
    fingerprint = compute_key_fingerprint(public_pem)

    space = params.get("space", "user")
    owner = params.get("owner", "local")

    store = TrustStore(project_path=Path(project_path))

    # Check if already trusted in this space
    existing = store.get_key(fingerprint)
    if existing and existing.source == space:
        return {
            "success": True,
            "fingerprint": fingerprint,
            "space": space,
            "already_trusted": True,
            "message": f"Key {fingerprint} already trusted in {space} space.",
        }

    result_fp = store.add_key(public_pem, owner=owner, space=space)

    # Determine where it was written
    if space == "project":
        trust_dir = Path(project_path) / AI_DIR / "trusted_keys"
    else:
        trust_dir = get_user_space() / AI_DIR / "trusted_keys"

    key_file = trust_dir / f"{result_fp}.toml"

    return {
        "success": True,
        "fingerprint": result_fp,
        "space": space,
        "owner": owner,
        "path": str(key_file),
        "signed": key_file.read_text().startswith("# rye:signed:"),
        "message": f"Key {result_fp} trusted in {space} space (owner: {owner}).",
    }


def _list(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """List all trusted keys across all spaces."""
    from rye.utils.trust_store import TrustStore

    store = TrustStore(project_path=Path(project_path))
    keys = store.list_keys()

    return {
        "success": True,
        "keys": [
            {
                "fingerprint": k.fingerprint,
                "owner": k.owner,
                "source": k.source,
                "attestation": k.attestation,
            }
            for k in keys
        ],
        "count": len(keys),
    }


def _remove(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Remove a key from the user trust store."""
    from rye.utils.trust_store import TrustStore

    fingerprint = params.get("fingerprint")
    if not fingerprint:
        return {"success": False, "error": "fingerprint is required for remove."}

    store = TrustStore(project_path=Path(project_path))
    removed = store.remove_key(fingerprint)

    return {
        "success": True,
        "fingerprint": fingerprint,
        "removed": removed,
        "message": (
            f"Removed key {fingerprint} from user trust store."
            if removed
            else f"Key {fingerprint} not found in user trust store."
        ),
    }
