"""Node key management — generate node identity and manage authorized keys.

Node identity lives at ~/.ai/node/identity/ (machine-local, never project space).
Authorized keys live at ~/.ai/node/authorized-keys/ (signed TOML files).

Actions:
  generate       - Generate a node Ed25519 keypair at ~/.ai/node/identity/
  info           - Show node fingerprint and public key
  authorize      - Add a public key to ~/.ai/node/authorized-keys/
  list           - List all authorized keys
  revoke         - Remove an authorized key by fingerprint
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/keys"
__tool_description__ = "Manage node identity and authorized keys at ~/.ai/node/"

from pathlib import Path
from typing import Any, Dict

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ["generate", "info", "authorize", "list", "revoke"],
            "description": (
                "Node key operation: generate (create node keypair), "
                "info (show node fingerprint), "
                "authorize (add key to authorized-keys), "
                "list (show all authorized keys), "
                "revoke (remove authorized key)"
            ),
        },
        "public_key_pem": {
            "type": "string",
            "description": "PEM-encoded Ed25519 public key for authorize action.",
        },
        "label": {
            "type": "string",
            "description": "Human-readable label for the authorized key. Default: unnamed.",
        },
        "scopes": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Access scopes for the authorized key. Default: ['*'].",
        },
        "fingerprint": {
            "type": "string",
            "description": "Key fingerprint for revoke action.",
        },
        "force": {
            "type": "boolean",
            "description": "Force regeneration even if node keypair exists.",
        },
    },
    "required": ["action"],
}


def execute(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Execute node key management action."""
    action = params.get("action")

    try:
        if action == "generate":
            return _generate(params)
        elif action == "info":
            return _info()
        elif action == "authorize":
            return _authorize(params)
        elif action == "list":
            return _list()
        elif action == "revoke":
            return _revoke(params)
        else:
            return {
                "success": False,
                "error": f"Unknown action: {action}. Valid: generate, info, authorize, list, revoke",
            }
    except Exception as e:
        return {"success": False, "error": str(e)}


def _generate(params: Dict[str, Any]) -> Dict[str, Any]:
    """Generate node Ed25519 keypair at ~/.ai/node/identity/."""
    from rye.constants import NodeDir
    from rye.primitives.signing import compute_key_fingerprint, ensure_keypair
    from rye.utils.path_utils import get_node_path

    identity_dir = get_node_path(NodeDir.IDENTITY)
    force = params.get("force", False)

    if force and identity_dir.exists():
        priv = identity_dir / "private_key.pem"
        pub = identity_dir / "public_key.pem"
        if priv.exists():
            priv.unlink()
        if pub.exists():
            pub.unlink()

    already_existed = (identity_dir / "private_key.pem").exists()
    private_pem, public_pem = ensure_keypair(identity_dir)
    fingerprint = compute_key_fingerprint(public_pem)

    return {
        "success": True,
        "fingerprint": fingerprint,
        "public_key_pem": public_pem.decode("utf-8").strip(),
        "identity_dir": str(identity_dir),
        "created": not already_existed,
        "message": (
            f"Node keypair already exists (fingerprint: {fingerprint})"
            if already_existed
            else f"Generated new node Ed25519 keypair (fingerprint: {fingerprint})"
        ),
    }


def _info() -> Dict[str, Any]:
    """Show node fingerprint and public key."""
    from rye.constants import NodeDir
    from rye.primitives.signing import compute_key_fingerprint, load_keypair
    from rye.utils.path_utils import get_node_path

    identity_dir = get_node_path(NodeDir.IDENTITY)

    if not (identity_dir / "private_key.pem").exists():
        return {
            "success": False,
            "error": "No node keypair found. Run action 'generate' first.",
            "identity_dir": str(identity_dir),
        }

    _, public_pem = load_keypair(identity_dir)
    fingerprint = compute_key_fingerprint(public_pem)

    return {
        "success": True,
        "fingerprint": fingerprint,
        "public_key_pem": public_pem.decode("utf-8").strip(),
        "identity_dir": str(identity_dir),
    }


def _authorize(params: Dict[str, Any]) -> Dict[str, Any]:
    """Add a public key to ~/.ai/node/authorized-keys/."""
    from rye.constants import NodeDir
    from rye.primitives.signing import load_keypair
    from rye.utils.authorized_keys import (
        build_and_sign_authorized_key,
        validate_label,
        validate_scopes,
    )
    from rye.utils.path_utils import get_node_path

    public_key_pem = params.get("public_key_pem")
    if not public_key_pem:
        return {"success": False, "error": "public_key_pem is required for authorize."}

    label = params.get("label", "unnamed")
    scopes = params.get("scopes", ["*"])

    try:
        validate_label(label)
        validate_scopes(scopes)
    except ValueError as e:
        return {"success": False, "error": str(e)}

    # Load node's own key for signing
    identity_dir = get_node_path(NodeDir.IDENTITY)
    if not (identity_dir / "private_key.pem").exists():
        return {
            "success": False,
            "error": "No node keypair found. Run 'generate' first.",
        }

    pub_pem_bytes = public_key_pem.encode("utf-8")
    node_priv, node_pub = load_keypair(identity_dir)

    signed_content, caller_fp = build_and_sign_authorized_key(
        public_key_pem=pub_pem_bytes,
        signer_private=node_priv,
        signer_public=node_pub,
        label=label,
        scopes=scopes,
    )

    # Write to authorized-keys dir
    auth_dir = get_node_path(NodeDir.AUTHORIZED_KEYS)
    key_file = auth_dir / f"{caller_fp}.toml"

    already_existed = key_file.exists()
    key_file.write_text(signed_content, encoding="utf-8")

    return {
        "success": True,
        "fingerprint": caller_fp,
        "label": label,
        "scopes": scopes,
        "path": str(key_file),
        "replaced": already_existed,
        "message": (
            f"Replaced authorized key {caller_fp} (label: {label})"
            if already_existed
            else f"Authorized key {caller_fp} added (label: {label})"
        ),
    }


def _list() -> Dict[str, Any]:
    """List all authorized keys at ~/.ai/node/authorized-keys/."""
    from rye.constants import NodeDir
    from rye.utils.path_utils import get_node_path

    try:
        import tomllib
    except ModuleNotFoundError:
        import tomli as tomllib  # type: ignore[no-redef]

    auth_dir = get_node_path(NodeDir.AUTHORIZED_KEYS)
    keys = []

    for f in sorted(auth_dir.glob("*.toml")):
        raw = f.read_text(encoding="utf-8")
        # Strip signature line
        lines = raw.split("\n", 1)
        body = lines[1] if len(lines) > 1 else raw
        try:
            data = tomllib.loads(body)
            keys.append({
                "fingerprint": data.get("fingerprint", f.stem),
                "label": data.get("label", ""),
                "scopes": data.get("scopes", []),
                "created_at": data.get("created_at", ""),
            })
        except Exception:
            keys.append({"fingerprint": f.stem, "label": "(parse error)", "scopes": []})

    return {"success": True, "keys": keys, "count": len(keys)}


def _revoke(params: Dict[str, Any]) -> Dict[str, Any]:
    """Remove an authorized key from ~/.ai/node/authorized-keys/."""
    from rye.constants import NodeDir
    from rye.utils.path_utils import get_node_path

    fingerprint = params.get("fingerprint")
    if not fingerprint:
        return {"success": False, "error": "fingerprint is required for revoke."}

    from rye.utils.authorized_keys import validate_fingerprint

    try:
        validate_fingerprint(fingerprint)
    except ValueError as e:
        return {"success": False, "error": str(e)}

    auth_dir = get_node_path(NodeDir.AUTHORIZED_KEYS)
    key_file = auth_dir / f"{fingerprint}.toml"

    if not key_file.exists():
        return {
            "success": False,
            "fingerprint": fingerprint,
            "message": f"Authorized key {fingerprint} not found.",
        }

    key_file.unlink()
    return {
        "success": True,
        "fingerprint": fingerprint,
        "message": f"Revoked authorized key {fingerprint}.",
    }
