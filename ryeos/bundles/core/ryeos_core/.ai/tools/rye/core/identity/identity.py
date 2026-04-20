# rye:signed:2026-04-20T05:37:45Z:12ea6acfb3ef45ab949c4ae90e4d6f788f9107550a6bd00e50f32e0bcbf3b766:IQ4-gEwjUiyTl7wBgLXuCS8FWyk2NMF6dioSf4pYy3aWSY4-fdmZEpFDbp5krkoKlZqiy6JPuckCAi7x-IwUCw:4b987fd4e40303ac
"""Identity management tool — create, show, and export identity documents.

An identity document (identity/v1) is a signed CAS object that binds an
Ed25519 signing key and X25519 encryption key together. The Ed25519 key
fingerprint IS the identity — principal_id is fp:<fingerprint>.

Actions:
  create - Generate identity document, sign it, store in CAS
  show   - Display current identity (fingerprint, public keys)
  export - Export identity document for publishing to registry/peers
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/identity"
__tool_description__ = "Manage identity documents (identity/v1)"

import base64
import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict

from rye.constants import AI_DIR

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ["create", "show", "export"],
            "description": "Identity operation: create, show, export",
        },
        "force": {
            "type": "boolean",
            "description": "Force recreation even if identity document exists.",
        },
    },
    "required": ["action"],
}


def execute(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Execute identity management action."""
    action = params.get("action")

    try:
        if action == "create":
            return _create(params, project_path)
        elif action == "show":
            return _show(params, project_path)
        elif action == "export":
            return _export(params, project_path)
        else:
            return {
                "success": False,
                "error": f"Unknown action: {action}. Valid: create, show, export",
            }
    except Exception as e:
        return {"success": False, "error": str(e)}


def _create(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Generate identity document, sign it, store in CAS."""
    from rye.primitives import cas
    from rye.primitives.signing import (
        compute_key_fingerprint,
        compute_box_fingerprint,
        ensure_full_keypair,
        sign_hash,
    )
    from rye.cas.store import user_cas_root
    from rye.utils.path_utils import get_signing_key_dir, get_user_space

    key_dir = get_signing_key_dir()
    force = params.get("force", False)

    if force and key_dir.exists():
        identity_ref = get_user_space() / AI_DIR / "config" / "identity.json"
        if identity_ref.exists():
            identity_ref.unlink()

    private_pem, public_pem, box_key, box_pub = ensure_full_keypair(key_dir)
    signing_fp = compute_key_fingerprint(public_pem)

    # Check for existing identity doc (unless force)
    identity_ref = get_user_space() / AI_DIR / "config" / "identity.json"
    if identity_ref.exists() and not force:
        ref_data = json.loads(identity_ref.read_text())
        return {
            "success": True,
            "principal_id": f"fp:{signing_fp}",
            "fingerprint": signing_fp,
            "object_hash": ref_data.get("hash"),
            "already_existed": True,
            "message": f"Identity already exists: fp:{signing_fp}",
        }

    # Build identity document
    signing_key_b64 = base64.urlsafe_b64encode(public_pem).rstrip(b"=").decode("ascii")
    box_pub_str = box_pub.decode("utf-8").strip()

    identity_doc = {
        "kind": "identity/v1",
        "principal_id": f"fp:{signing_fp}",
        "signing_key": f"ed25519:{signing_key_b64}",
        "box_key": f"x25519:{box_pub_str}",
        "services": [],
        "created_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }

    # Sign the identity document
    payload = json.dumps(
        {k: v for k, v in identity_doc.items() if k != "_signature"},
        sort_keys=True,
        separators=(",", ":"),
    )
    content_hash = hashlib.sha256(payload.encode()).hexdigest()
    sig_b64 = sign_hash(content_hash, private_pem)
    signed_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    identity_doc["_signature"] = {
        "signer": f"fp:{signing_fp}",
        "sig": sig_b64,
        "signed_at": signed_at,
    }

    # Store in CAS
    cas_root = user_cas_root()
    object_hash = cas.store_object(identity_doc, cas_root)

    # Write ref so we can find the current identity
    identity_ref.parent.mkdir(parents=True, exist_ok=True)
    identity_ref.write_text(json.dumps({
        "hash": object_hash,
        "fingerprint": signing_fp,
    }))

    return {
        "success": True,
        "principal_id": f"fp:{signing_fp}",
        "fingerprint": signing_fp,
        "object_hash": object_hash,
        "message": f"Identity created: fp:{signing_fp}",
    }


def _show(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Display current identity (fingerprint, public keys)."""
    from rye.primitives.signing import (
        compute_key_fingerprint,
        compute_box_fingerprint,
        load_box_keypair,
        load_keypair,
    )
    from rye.utils.path_utils import get_signing_key_dir, get_user_space

    key_dir = get_signing_key_dir()

    if not (key_dir / "private_key.pem").exists():
        return {
            "success": False,
            "error": "No keypair found. Run keys generate first.",
        }

    _, public_pem = load_keypair(key_dir)
    signing_fp = compute_key_fingerprint(public_pem)

    # Check for box keys
    box_info: Dict[str, Any] = {}
    try:
        _, box_pub = load_box_keypair(key_dir)
        box_fp = compute_box_fingerprint(box_pub)
        box_info = {
            "box_fingerprint": box_fp,
            "has_box_key": True,
        }
    except FileNotFoundError:
        box_info = {"has_box_key": False}

    # Check for stored identity doc
    identity_ref = get_user_space() / AI_DIR / "config" / "identity.json"
    identity_hash = None
    if identity_ref.exists():
        ref_data = json.loads(identity_ref.read_text())
        identity_hash = ref_data.get("hash")

    return {
        "success": True,
        "principal_id": f"fp:{signing_fp}",
        "fingerprint": signing_fp,
        "public_key_pem": public_pem.decode("utf-8").strip(),
        "identity_hash": identity_hash,
        **box_info,
    }


def _export(params: Dict[str, Any], project_path: str) -> Dict[str, Any]:
    """Export identity document for publishing to registry/peers."""
    from rye.primitives import cas
    from rye.cas.store import user_cas_root
    from rye.utils.path_utils import get_user_space

    identity_ref = get_user_space() / AI_DIR / "config" / "identity.json"
    if not identity_ref.exists():
        return {
            "success": False,
            "error": "No identity found. Run create first.",
        }

    ref_data = json.loads(identity_ref.read_text())
    object_hash = ref_data["hash"]

    identity_doc = cas.get_object(object_hash, user_cas_root())
    if identity_doc is None:
        return {
            "success": False,
            "error": f"Identity object {object_hash} not found in CAS.",
        }

    return {
        "success": True,
        "identity": identity_doc,
        "object_hash": object_hash,
    }
