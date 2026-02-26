# rye:signed:2026-02-26T05:52:24Z:4e03ca5e8af1835a72638d0fddbac4855ced6dc3c23880a020326eebe635ec84:r5wAT-dP_ZoAXyqJwCbtRa4PCFdHtW3WN9ZXDPbVsWPpaxQwX0L2aU3x2KSJNuZ0wY2jU_Y1ChZXyKxsnb2jAA==:4b987fd4e40303ac
"""Key management tool — generate, inspect, and trust Ed25519 signing keys.

The user's signing identity. Handles keypair generation, fingerprint display,
and trusted key provisioning into project or user space.

Actions:
  generate - Create a new Ed25519 keypair (or return existing)
  import   - Import a private key from an environment variable (for CI/serverless)
  info     - Show current key fingerprint and public key
  trust    - Add the current key to a space's config/keys/trusted (signed TOML)
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
            "enum": ["generate", "import", "info", "trust", "list", "remove"],
            "description": (
                "Key operation: generate (create keypair), "
                "import (inject private key from env var for CI/serverless), "
                "info (show fingerprint), trust (add key to space), "
                "list (show all trusted keys), remove (remove from user trust store)"
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
        "env_var": {
            "type": "string",
            "description": "Environment variable containing private key PEM for import action. Default: RYE_SIGNING_KEY.",
        },
        "auto_trust": {
            "type": "boolean",
            "description": "Automatically trust the imported key in user space. Default: true.",
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
        elif action == "import":
            return _import(params, project_path)
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
                "error": f"Unknown action: {action}. Valid: generate, import, info, trust, list, remove",
            }
    except Exception as e:
        return {"success": False, "error": str(e)}


def _generate(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Generate an Ed25519 keypair (or return existing)."""
    from lillux.primitives.signing import ensure_keypair, compute_key_fingerprint
    from rye.utils.path_utils import get_user_space

    key_dir = get_user_space() / AI_DIR / "config" / "keys" / "signing"
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


def _import(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Import a private key from an environment variable.

    Reads an Ed25519 private key PEM from an env var, derives the public key,
    writes both to ~/.ai/config/keys/signing/, and optionally trusts the key in user space.
    Designed for CI/CD and serverless containers.
    """
    import os
    from cryptography.hazmat.primitives import serialization
    from lillux.primitives.signing import (
        save_keypair,
        compute_key_fingerprint,
    )
    from rye.utils.path_utils import get_user_space

    env_var = params.get("env_var", "RYE_SIGNING_KEY")
    auto_trust = params.get("auto_trust", True)

    raw_value = os.environ.get(env_var)
    if not raw_value:
        return {
            "success": False,
            "error": f"Environment variable {env_var} is not set.",
            "hint": f"Export your private key: export {env_var}=\"$(cat ~/.ai/config/keys/signing/private_key.pem)\"",
        }

    # Normalize: env vars often have literal \n instead of newlines
    private_pem = raw_value.replace("\\n", "\n").encode("utf-8")

    # Validate and derive public key
    try:
        private_key = serialization.load_pem_private_key(private_pem, password=None)
    except Exception as e:
        return {
            "success": False,
            "error": f"Invalid private key in {env_var}: {e}",
            "hint": "Value must be a PEM-encoded Ed25519 private key.",
        }

    public_key = private_key.public_key()
    public_pem = public_key.public_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PublicFormat.SubjectPublicKeyInfo,
    )

    # Write to key directory
    key_dir = get_user_space() / AI_DIR / "config" / "keys" / "signing"
    save_keypair(private_pem, public_pem, key_dir)
    fingerprint = compute_key_fingerprint(public_pem)

    result = {
        "success": True,
        "fingerprint": fingerprint,
        "public_key_pem": public_pem.decode("utf-8").strip(),
        "key_dir": str(key_dir),
        "env_var": env_var,
        "message": f"Imported signing key from ${env_var} (fingerprint: {fingerprint})",
    }

    # Auto-trust in user space
    if auto_trust:
        from rye.utils.trust_store import TrustStore

        store = TrustStore(project_path=Path(project_path))
        existing = store.get_key(fingerprint)
        if existing:
            result["trusted"] = True
            result["trust_message"] = f"Key {fingerprint} already trusted in {existing.source} space."
        else:
            store.add_key(public_pem, owner="local", space="user")
            result["trusted"] = True
            result["trust_message"] = f"Key {fingerprint} trusted in user space."

    return result


def _info(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Show current key fingerprint and public key."""
    from lillux.primitives.signing import ensure_keypair, compute_key_fingerprint
    from rye.utils.path_utils import get_user_space

    key_dir = get_user_space() / AI_DIR / "config" / "keys" / "signing"

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
    """Add the current signing key to a space's config/keys/trusted."""
    from lillux.primitives.signing import ensure_keypair, compute_key_fingerprint
    from rye.utils.path_utils import get_user_space
    from rye.utils.trust_store import TrustStore

    key_dir = get_user_space() / AI_DIR / "config" / "keys" / "signing"

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
        trust_dir = Path(project_path) / AI_DIR / "config" / "keys" / "trusted"
    else:
        trust_dir = get_user_space() / AI_DIR / "config" / "keys" / "trusted"

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
