"""First-boot initialization for ryeos-node.

Scaffolds node space: signing key, authorized_keys dir, node.yaml.
Idempotent — safe to call on every startup.

Usage:
    python -m ryeos_node.init /cas
    python -m ryeos_node.init ~/.ai/node
"""

import logging
import os
import sys
from pathlib import Path

logger = logging.getLogger(__name__)


def _bootstrap_trust_store(pub_pem: bytes, fp: str, label: str, signing_dir: Path) -> None:
    """Add a public key to the node's user-level trust store.

    This ensures the execution engine (TrustStore) trusts tools
    signed by this key, not just the API auth layer.
    """
    from rye.utils.trust_store import TrustStore

    try:
        trust_store = TrustStore()
        trust_store.add_key(
            public_key_pem=pub_pem,
            owner=label,
            version="1.0.0",
        )
        logger.info("Bootstrapped trust store key: fp:%s (owner=%s)", fp, label)
    except Exception:
        logger.warning("Failed to bootstrap trust store for fp:%s", fp, exc_info=True)


def _bootstrap_authorized_key(authorized_keys_dir: Path, signing_dir: Path) -> None:
    """Create first authorized key from BOOTSTRAP_PUBLIC_KEY env var.

    Env vars:
        BOOTSTRAP_PUBLIC_KEY  — required, format: "ed25519:<base64>" (the PEM content, base64-encoded)
        BOOTSTRAP_LABEL       — optional, human label (default: "bootstrap")

    Only acts when the authorized_keys dir is empty (first boot).
    The key file is signed by the node's own signing key.
    Fingerprint is derived from the public key server-side.
    """
    pub_key_env = os.environ.get("BOOTSTRAP_PUBLIC_KEY", "").strip()
    if not pub_key_env:
        return

    # Skip if any keys already exist
    existing = [f for f in authorized_keys_dir.iterdir() if f.suffix == ".toml"]
    if existing:
        return

    if not pub_key_env.startswith("ed25519:"):
        logger.warning("BOOTSTRAP_PUBLIC_KEY must start with 'ed25519:', skipping")
        return

    import base64
    from rye.primitives.signing import (
        compute_key_fingerprint,
        load_keypair,
    )
    from rye.utils.authorized_keys import (
        build_authorized_key_body,
        sign_authorized_key,
    )

    # Derive fingerprint from the provided public key
    pub_pem = base64.b64decode(pub_key_env[len("ed25519:"):])
    fp = compute_key_fingerprint(pub_pem)
    label = os.environ.get("BOOTSTRAP_LABEL", "bootstrap").strip()

    body, timestamp = build_authorized_key_body(
        fingerprint=fp,
        public_key_encoded=pub_key_env,
        label=label,
        scopes=["*"],
        extra_fields={"created_via": "bootstrap_env"},
    )

    node_priv, node_pub = load_keypair(signing_dir)
    signed_content = sign_authorized_key(body, timestamp, node_priv, node_pub)

    key_file = authorized_keys_dir / f"{fp}.toml"
    key_file.write_text(signed_content, encoding="utf-8")
    logger.info("Bootstrapped authorized key: fp:%s (label=%s)", fp, label)

    # Also register in the CAS trust store so the execution engine
    # trusts tools signed by this key (authorized_keys is API-only).
    _bootstrap_trust_store(pub_pem, fp, label, signing_dir)


def ensure_node_space(cas_base_path: str) -> str:
    """Initialize node space under cas_base_path. Returns node fingerprint."""
    cas = Path(cas_base_path)
    signing_dir = cas / "signing"
    config_root = cas / "config"
    authorized_keys_dir = config_root / "authorized_keys"
    ai_dir = os.environ.get("AI_DIR", ".ai")
    node_yaml_dir = config_root / ai_dir / "config" / "node"
    node_yaml_path = node_yaml_dir / "node.yaml"

    # 1. Generate signing key if missing
    signing_dir.mkdir(parents=True, exist_ok=True)
    from rye.primitives.signing import (
        compute_key_fingerprint,
        ensure_full_keypair,
    )

    _, pub, _, _ = ensure_full_keypair(signing_dir)
    fingerprint = compute_key_fingerprint(pub)

    # 2. Create authorized_keys dir + bootstrap first authorized key
    authorized_keys_dir.mkdir(parents=True, exist_ok=True)
    _bootstrap_authorized_key(authorized_keys_dir, signing_dir)

    # 3. Scaffold node.yaml if missing
    if not node_yaml_path.exists():
        node_yaml_dir.mkdir(parents=True, exist_ok=True)
        node_yaml_path.write_text(
            f"identity:\n"
            f"  name: node-{fingerprint[:8]}\n"
            f"  signing_key_dir: {signing_dir}\n"
            f"hardware:\n"
            f"  gpus: 0\n"
            f"  memory_gb: 2\n"
            f"features:\n"
            f"  registry: true\n"
            f"  webhooks: true\n"
            f"limits:\n"
            f"  max_concurrent: 8\n"
            f"coordination:\n"
            f"  type: asyncio\n",
            encoding="utf-8",
        )
        logger.info("Created node.yaml at %s", node_yaml_path)

    # 4. Log node fingerprint
    logger.info("Node fingerprint: fp:%s", fingerprint)
    return fingerprint


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    cas_base_path = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("CAS_BASE_PATH", "/cas")
    fp = ensure_node_space(cas_base_path)
    print(f"Node ready — fp:{fp}")
    print(f"CAS: {cas_base_path}")
    print(f"Config: {Path(cas_base_path) / 'config'}")
    print(f"Add authorized keys to: {Path(cas_base_path) / 'config' / 'authorized_keys' / '<fingerprint>.toml'}")


if __name__ == "__main__":
    main()
