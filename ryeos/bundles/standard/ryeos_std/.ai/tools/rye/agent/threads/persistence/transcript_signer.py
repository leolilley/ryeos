# rye:signed:2026-02-26T03:49:32Z:139be71fcfb0c3cf1796442383b97dc6ed4f099b419afdafba422586619c8383:ZI0hWs-n2JHnm3MI_47SoG5oXuKYCIZaJfBLOA75Sy5QOks9neesBTS5UiqS_UE2TQ3XkR7X0sC613FNFKrsDw==:9fbfabe975fa5a7f
"""Checkpoint signing for transcript integrity and JSON signing utilities.

Signs transcript.jsonl at turn boundaries by appending checkpoint events
to the JSONL stream. Each checkpoint's hash covers all bytes before the
checkpoint line (byte_offset = start of checkpoint line).

Also provides sign_json / verify_json for JSON files (thread.json) using
a _signature field with canonical serialization.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "Transcript checkpoint signing and JSON signing utilities"

import hashlib
import json
import logging
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, Optional

logger = logging.getLogger(__name__)


def _get_keypair():
    """Load or generate the user's Ed25519 keypair."""
    from rye.constants import AI_DIR
    from rye.utils.path_utils import get_user_space
    from lillux.primitives.signing import ensure_keypair

    key_dir = get_user_space() / AI_DIR / "keys"
    return ensure_keypair(key_dir)


def _ensure_self_trusted(public_pem: bytes, fingerprint: str) -> None:
    """Auto-trust own public key if not already trusted."""
    from rye.utils.trust_store import TrustStore

    trust_store = TrustStore()
    if not trust_store.is_trusted(fingerprint):
        trust_store.add_key(public_pem, owner="self")


class TranscriptSigner:
    """Checkpoint signing for transcript integrity.

    Signs transcript.jsonl at turn boundaries by appending a checkpoint
    event to the JSONL stream. Each checkpoint's hash covers all bytes
    before the checkpoint line (byte_offset = start of checkpoint line).

    Verification reads the JSONL, extracts checkpoint events, and verifies
    each hash + signature against the file content.
    """

    def __init__(self, thread_id: str, thread_dir: Path):
        self._thread_id = thread_id
        self._jsonl_path = thread_dir / "transcript.jsonl"

    def checkpoint(self, turn: int) -> None:
        """Sign the transcript up to its current size.

        Called by runner.py at turn boundaries. The checkpoint event is
        appended to the same JSONL file as all other events.
        """
        if not self._jsonl_path.exists():
            return

        from lillux.primitives.signing import sign_hash, compute_key_fingerprint

        private_pem, public_pem = _get_keypair()
        pubkey_fp = compute_key_fingerprint(public_pem)
        _ensure_self_trusted(public_pem, pubkey_fp)

        byte_offset = self._jsonl_path.stat().st_size
        content = self._jsonl_path.read_bytes()
        content_hash = hashlib.sha256(content).hexdigest()

        ed25519_sig = sign_hash(content_hash, private_pem)
        ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

        entry = {
            "timestamp": time.time(),
            "thread_id": self._thread_id,
            "event_type": "checkpoint",
            "payload": {
                "turn": turn,
                "byte_offset": byte_offset,
                "hash": content_hash,
                "sig": ed25519_sig,
                "fp": pubkey_fp,
                "ts": ts,
            },
        }
        with open(self._jsonl_path, "a") as f:
            f.write(json.dumps(entry, default=str) + "\n")
            f.flush()

    def verify(self, *, allow_unsigned_trailing: bool = False) -> Dict:
        """Verify the transcript against its checkpoint events.

        Reads the JSONL, extracts checkpoint events, and verifies each
        hash + Ed25519 signature against the file content at that byte offset.

        Args:
            allow_unsigned_trailing: If True, unsigned trailing content
                after the last checkpoint is allowed (lenient mode).

        Returns:
            {"valid": True, "checkpoints": N} on success.
            {"valid": False, "error": "...", "failed_at_turn": N} on failure.
        """
        if not self._jsonl_path.exists():
            return {"valid": True, "checkpoints": 0, "unsigned": True}

        content = self._jsonl_path.read_bytes()

        checkpoints = []
        for line in content.decode("utf-8").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
                if event.get("event_type") == "checkpoint":
                    checkpoints.append(event["payload"])
            except json.JSONDecodeError:
                continue

        if not checkpoints:
            return {"valid": True, "checkpoints": 0, "unsigned": True}

        from lillux.primitives.signing import verify_signature
        from rye.utils.trust_store import TrustStore

        trust_store = TrustStore()

        for cp in checkpoints:
            byte_offset = cp["byte_offset"]
            expected_hash = cp["hash"]

            actual_hash = hashlib.sha256(content[:byte_offset]).hexdigest()
            if actual_hash != expected_hash:
                return {
                    "valid": False,
                    "error": f"Content hash mismatch at turn {cp['turn']}",
                    "failed_at_turn": cp["turn"],
                    "byte_offset": byte_offset,
                }

            key_info = trust_store.get_key(cp["fp"])
            if key_info is None:
                return {
                    "valid": False,
                    "error": f"Untrusted signing key {cp['fp']} at turn {cp['turn']}",
                    "failed_at_turn": cp["turn"],
                }
            public_key_pem = getattr(key_info, "public_key_pem", key_info)

            if not verify_signature(expected_hash, cp["sig"], public_key_pem):
                return {
                    "valid": False,
                    "error": f"Signature verification failed at turn {cp['turn']}",
                    "failed_at_turn": cp["turn"],
                }

        # Check for unsigned trailing content
        last_cp = checkpoints[-1]
        last_cp_end = content.find(b"\n", last_cp["byte_offset"]) + 1
        if last_cp_end > 0 and last_cp_end < len(content):
            trailing_bytes = len(content) - last_cp_end
            if not allow_unsigned_trailing:
                return {
                    "valid": False,
                    "error": (
                        f"Unsigned content after last checkpoint "
                        f"({trailing_bytes} bytes after turn {last_cp['turn']})"
                    ),
                    "failed_at_turn": last_cp["turn"],
                    "unsigned_bytes": trailing_bytes,
                }
            logger.warning(
                "Unsigned trailing content: %d bytes after turn %d",
                trailing_bytes,
                last_cp["turn"],
            )

        return {"valid": True, "checkpoints": len(checkpoints)}


# --- JSON signing utilities ---


def sign_json(data: dict) -> dict:
    """Sign a JSON-serializable dict. Adds _signature field.

    Uses canonical serialization (sorted keys, compact separators)
    so the hash is reproducible on verification.
    """
    from lillux.primitives.signing import sign_hash, compute_key_fingerprint

    private_pem, public_pem = _get_keypair()
    pubkey_fp = compute_key_fingerprint(public_pem)
    _ensure_self_trusted(public_pem, pubkey_fp)

    content = {k: v for k, v in data.items() if k != "_signature"}
    canonical = json.dumps(content, sort_keys=True, separators=(",", ":"))
    content_hash = hashlib.sha256(canonical.encode()).hexdigest()

    ed25519_sig = sign_hash(content_hash, private_pem)
    ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    data["_signature"] = f"rye:signed:{ts}:{content_hash}:{ed25519_sig}:{pubkey_fp}"
    return data


def _parse_signature_str(sig_str: str) -> Optional[dict]:
    """Parse a rye:signed:... string into its components.

    Reuses the same regex pattern as MetadataManager._SIGNED_FIELDS
    to handle the colon-containing ISO timestamp correctly.
    """
    import re
    from rye.utils.metadata_manager import _SIGNED_FIELDS

    m = re.match(r"rye:signed:" + _SIGNED_FIELDS + r"$", sig_str)
    if not m:
        return None
    return {
        "timestamp": m.group(1),
        "hash": m.group(2),
        "ed25519_sig": m.group(3),
        "pubkey_fp": m.group(4),
    }


def verify_json(data: dict) -> bool:
    """Verify a signed JSON dict.

    Returns True if signature is valid, False otherwise.
    """
    sig_str = data.get("_signature")
    if not sig_str:
        return False

    parsed = _parse_signature_str(sig_str)
    if not parsed:
        return False

    from lillux.primitives.signing import verify_signature
    from rye.utils.trust_store import TrustStore

    content = {k: v for k, v in data.items() if k != "_signature"}
    canonical = json.dumps(content, sort_keys=True, separators=(",", ":"))
    actual_hash = hashlib.sha256(canonical.encode()).hexdigest()

    if actual_hash != parsed["hash"]:
        return False

    trust_store = TrustStore()
    key_info = trust_store.get_key(parsed["pubkey_fp"])
    if key_info is None:
        return False

    return verify_signature(parsed["hash"], parsed["ed25519_sig"], key_info.public_key_pem)
