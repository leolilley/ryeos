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
    from lillux.primitives.signing import (
        compute_key_fingerprint,
        ensure_full_keypair,
    )

    _, pub, _, _ = ensure_full_keypair(signing_dir)
    fingerprint = compute_key_fingerprint(pub)

    # 2. Create authorized_keys dir
    authorized_keys_dir.mkdir(parents=True, exist_ok=True)

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
            f"  registry: false\n"
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
