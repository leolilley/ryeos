"""First-boot initialization for ryeos-node.

Scaffolds node space: signing key, authorized_keys dir, node.yaml.
Idempotent — safe to call on every startup.

Usage:
    python -m ryeos_node.init /cas
    python -m ryeos_node.init ~/.ryeos-node
"""

import logging
import os
import sys
from pathlib import Path

logger = logging.getLogger(__name__)


def _bootstrap_authorized_key(authorized_keys_dir: Path, signing_dir: Path) -> None:
    """Create first authorized key from BOOTSTRAP_AUTHORIZED_KEY env var.

    Format: "fp:<fingerprint>" or just "<fingerprint>"
    Optional: "fp:<fingerprint>:<owner>" to set owner name (defaults to "bootstrap")

    Only acts when the authorized_keys dir is empty (first boot).
    The key file is signed by the node's own signing key.
    """
    bootstrap = os.environ.get("BOOTSTRAP_AUTHORIZED_KEY", "").strip()
    if not bootstrap:
        return

    # Skip if any keys already exist
    existing = [f for f in authorized_keys_dir.iterdir() if f.suffix == ".toml"]
    if existing:
        return

    # Parse: fp:abc123:owner or fp:abc123 or abc123
    parts = bootstrap.split(":")
    if parts[0] == "fp":
        parts = parts[1:]
    fp = parts[0]
    owner = parts[1] if len(parts) > 1 else "bootstrap"

    if not fp:
        logger.warning("BOOTSTRAP_AUTHORIZED_KEY set but empty fingerprint, skipping")
        return

    import hashlib
    import time
    from rye.primitives.signing import (
        compute_key_fingerprint,
        load_keypair,
        sign_hash,
    )

    caps_toml = '"rye.*"'
    body = (
        f'fingerprint = "{fp}"\n'
        f'owner = "{owner}"\n'
        f'capabilities = [{caps_toml}]\n'
    )

    node_priv, node_pub = load_keypair(signing_dir)
    node_fp = compute_key_fingerprint(node_pub)
    content_hash = hashlib.sha256(body.encode()).hexdigest()
    sig_b64 = sign_hash(content_hash, node_priv)
    timestamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

    signed_content = f"# rye:signed:{timestamp}:{content_hash}:{sig_b64}:{node_fp}\n{body}"

    key_file = authorized_keys_dir / f"{fp}.toml"
    key_file.write_text(signed_content, encoding="utf-8")
    logger.info("Bootstrapped authorized key: fp:%s (owner=%s)", fp, owner)


def ensure_node_space(cas_base_path: str) -> str:
    """Initialize node space under cas_base_path. Returns node fingerprint."""
    cas = Path(cas_base_path)
    signing_dir = cas / "signing"
    config_root = cas / "config"
    authorized_keys_dir = config_root / "authorized_keys"
    node_yaml_dir = config_root / ".ai" / "config" / "node"
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
