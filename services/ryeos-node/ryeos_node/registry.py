"""CAS-native registry index management.

Pure filesystem operations — no FastAPI dependencies.
Registry index is a CAS object with a head ref managed
via rye/cas/refs.py primitives.
"""

import base64
import datetime
import fcntl
import hashlib
import json
import logging
import os
from pathlib import Path

from lillux.primitives import cas
from lillux.primitives.signing import verify_signature
from rye.cas.refs import read_ref, write_ref_atomic

logger = logging.getLogger(__name__)

VALID_ITEM_TYPES = ("tool", "directive", "knowledge")


# ---------------------------------------------------------------------------
# Path helpers
# ---------------------------------------------------------------------------

def _registry_dir(cas_base: str) -> Path:
    return Path(cas_base) / "registry"


def _index_head_path(cas_base: str) -> Path:
    return _registry_dir(cas_base) / "index" / "head"


def _cas_root(cas_base: str) -> Path:
    return _registry_dir(cas_base) / "objects"


def _namespace_dir(cas_base: str) -> Path:
    return _registry_dir(cas_base) / "namespaces"


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


def _empty_index() -> dict:
    return {
        "kind": "registry-index/v1",
        "schema": 1,
        "updated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "entries": {"tool": {}, "directive": {}, "knowledge": {}},
    }


def _compute_content_hash(obj: dict) -> str:
    payload = json.dumps(
        {k: v for k, v in obj.items() if k != "_signature"},
        sort_keys=True,
        separators=(",", ":"),
    )
    return hashlib.sha256(payload.encode()).hexdigest()


def _extract_public_key_pem(signing_key: str) -> bytes:
    """Extract PEM bytes from 'ed25519:<b64>' format."""
    if not signing_key.startswith("ed25519:"):
        raise ValueError("Invalid signing_key format")
    return base64.b64decode(signing_key[8:])


def _verify_signed_object(obj: dict, public_key_pem: bytes) -> bool:
    sig_block = obj.get("_signature")
    if not sig_block:
        return False
    sig = sig_block.get("sig", "")
    if not sig:
        return False
    content_hash = _compute_content_hash(obj)
    return verify_signature(content_hash, sig, public_key_pem)


# ---------------------------------------------------------------------------
# Index operations
# ---------------------------------------------------------------------------

def load_index(cas_base: str) -> dict:
    head = read_ref(_index_head_path(cas_base))
    if head is None:
        return _empty_index()
    obj = cas.get_object(head, _cas_root(cas_base))
    if obj is None:
        logger.warning("Index head %s points to missing object, returning empty", head)
        return _empty_index()
    return obj


def verify_namespace_owner(cas_base: str, namespace: str, publisher_fp: str) -> str | None:
    """Check publisher owns the namespace. Returns error string or None."""
    ns_file = _namespace_dir(cas_base) / namespace
    if not ns_file.exists():
        return f"Namespace '{namespace}' not claimed. Claim it first via /registry/namespaces/claim"
    record = json.loads(ns_file.read_bytes())
    if record.get("owner") != publisher_fp:
        return f"Publisher {publisher_fp} does not own namespace '{namespace}'"
    return None


def publish_item(
    cas_base: str,
    item_type: str,
    item_id: str,
    version: str,
    manifest_hash: str,
    publisher_fp: str,
) -> dict:
    if item_type not in VALID_ITEM_TYPES:
        return {"ok": False, "error": f"Invalid item_type: {item_type}"}
    if not item_id or not version or not manifest_hash or not publisher_fp:
        return {"ok": False, "error": "Missing required field"}

    namespace = item_id.split("/")[0] if "/" in item_id else item_id

    # Verify publisher owns the namespace
    ns_err = verify_namespace_owner(cas_base, namespace, publisher_fp)
    if ns_err:
        return {"ok": False, "error": ns_err}

    lock_path = _registry_dir(cas_base) / "index.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    with open(lock_path, "w") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        try:
            index = load_index(cas_base)
            type_entries = index["entries"].setdefault(item_type, {})

            if item_id in type_entries:
                entry = type_entries[item_id]
                if version in entry["versions"]:
                    existing = entry["versions"][version]
                    if existing["manifest_hash"] == manifest_hash:
                        logger.info("Idempotent skip: %s/%s@%s", item_type, item_id, version)
                        head = read_ref(_index_head_path(cas_base))
                        return {"ok": True, "head": head, "skipped": True}
                    return {
                        "ok": False,
                        "error": f"Version {version} already exists with different hash",
                    }
            else:
                type_entries[item_id] = {
                    "namespace": namespace,
                    "owner": publisher_fp,
                    "latest_version": version,
                    "versions": {},
                }
                entry = type_entries[item_id]

            now = datetime.datetime.now(datetime.timezone.utc).isoformat()
            entry["versions"][version] = {
                "manifest_hash": manifest_hash,
                "published_at": now,
                "publisher": publisher_fp,
            }
            entry["latest_version"] = version

            index["updated_at"] = now
            new_head = cas.store_object(index, _cas_root(cas_base))
            write_ref_atomic(_index_head_path(cas_base), new_head)

            logger.info("Published %s/%s@%s -> %s", item_type, item_id, version, new_head)
            return {"ok": True, "head": new_head}
        finally:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_UN)


# ---------------------------------------------------------------------------
# Query operations
# ---------------------------------------------------------------------------

def search_items(
    cas_base: str,
    query: str | None = None,
    item_type: str | None = None,
    namespace: str | None = None,
    limit: int = 20,
) -> list[dict]:
    index = load_index(cas_base)
    results: list[dict] = []

    types_to_search = [item_type] if item_type and item_type in VALID_ITEM_TYPES else list(VALID_ITEM_TYPES)

    for t in types_to_search:
        for iid, entry in index["entries"].get(t, {}).items():
            if namespace and entry.get("namespace") != namespace:
                continue
            if query and query.lower() not in iid.lower():
                continue
            results.append({
                "item_type": t,
                "item_id": iid,
                "namespace": entry.get("namespace"),
                "latest_version": entry.get("latest_version"),
                "owner": entry.get("owner"),
            })
            if len(results) >= limit:
                return results

    return results


def get_item(cas_base: str, item_type: str, item_id: str) -> dict | None:
    index = load_index(cas_base)
    return index["entries"].get(item_type, {}).get(item_id)


def get_version(
    cas_base: str, item_type: str, item_id: str, version: str
) -> dict | None:
    item = get_item(cas_base, item_type, item_id)
    if item is None:
        return None
    return item.get("versions", {}).get(version)


# ---------------------------------------------------------------------------
# Namespace claims
# ---------------------------------------------------------------------------

def claim_namespace(cas_base: str, signed_claim: dict) -> dict:
    if signed_claim.get("kind") != "namespace-claim/v1":
        return {"ok": False, "error": "Invalid kind, expected namespace-claim/v1"}

    ns = signed_claim.get("namespace")
    if not ns:
        return {"ok": False, "error": "Missing namespace"}

    sig_block = signed_claim.get("_signature")
    if not sig_block:
        return {"ok": False, "error": "Missing signature"}

    signer_fp = sig_block.get("signer", "")
    if not signer_fp.startswith("fp:"):
        return {"ok": False, "error": "Invalid signer format"}

    # Look up signer's identity to get public key
    identity = lookup_identity(cas_base, signer_fp[3:])
    if identity is None:
        return {"ok": False, "error": "Signer identity not found"}

    public_key_pem = _extract_public_key_pem(identity["signing_key"])
    if not _verify_signed_object(signed_claim, public_key_pem):
        return {"ok": False, "error": "Invalid signature"}

    # First-come-first-served
    ns_dir = _namespace_dir(cas_base)
    ns_file = ns_dir / ns
    if ns_file.exists():
        existing = json.loads(ns_file.read_bytes())
        if existing.get("owner") != signed_claim.get("owner"):
            return {"ok": False, "error": f"Namespace '{ns}' already claimed"}
        return {"ok": True, "skipped": True}

    claim_hash = cas.store_object(signed_claim, _cas_root(cas_base))
    record = {"owner": signed_claim.get("owner"), "claim_hash": claim_hash}
    _atomic_write(ns_file, json.dumps(record).encode())

    logger.info("Namespace '%s' claimed by %s", ns, signed_claim.get("owner"))
    return {"ok": True, "claim_hash": claim_hash}


# ---------------------------------------------------------------------------
# Identity registration
# ---------------------------------------------------------------------------

def register_identity(cas_base: str, identity_doc: dict) -> dict:
    if identity_doc.get("kind") != "identity/v1":
        return {"ok": False, "error": "Invalid kind, expected identity/v1"}

    signing_key = identity_doc.get("signing_key", "")
    if not signing_key.startswith("ed25519:"):
        return {"ok": False, "error": "Invalid signing_key format"}

    principal_id = identity_doc.get("principal_id", "")
    if not principal_id.startswith("fp:"):
        return {"ok": False, "error": "Invalid principal_id format"}

    # Self-signed: verify using the key embedded in the doc
    public_key_pem = _extract_public_key_pem(signing_key)
    if not _verify_signed_object(identity_doc, public_key_pem):
        return {"ok": False, "error": "Invalid self-signature"}

    identity_hash = cas.store_object(identity_doc, _cas_root(cas_base))

    # Fingerprint → hash mapping
    fingerprint = principal_id[3:]
    id_dir = _registry_dir(cas_base) / "identities"
    _atomic_write(id_dir / fingerprint, identity_hash.encode())

    logger.info("Registered identity %s -> %s", fingerprint, identity_hash)
    return {"ok": True, "identity_hash": identity_hash}


def lookup_identity(cas_base: str, fingerprint: str) -> dict | None:
    id_file = _registry_dir(cas_base) / "identities" / fingerprint
    if not id_file.exists():
        return None
    identity_hash = id_file.read_text().strip()
    return cas.get_object(identity_hash, _cas_root(cas_base))
