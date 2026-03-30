"""
ryeos-node — Modal Deployment

CAS-native remote execution server on Modal.
Volume-backed CAS storage, persistent signing key, per-user isolation.

Usage:
  modal deploy modal_app.py          # Deploy to Modal
  modal serve modal_app.py           # Local dev with Modal

Packages: ryeos-core (from PyPI) provides the engine, CAS, and core bundle.
Only ryeos_node/ is copied locally (server, auth, config).
"""

import modal

app = modal.App("ryeos-node")

cas_volume = modal.Volume.from_name("ryeos-node-cas", create_if_missing=True)

image = (
    modal.Image.debian_slim(python_version="3.12")
    .pip_install(
        # Standard bundle (ryeos → ryeos-core → ryeos-engine → lillux)
        # Includes thread_directive for directive forking on remote
        "ryeos>=0.1.20",
        # Server
        "fastapi>=0.109.0",
        "uvicorn[standard]>=0.27.0",
        "pydantic-settings>=2.1.0",
        "pyyaml>=6.0",
        # Auth (TOML parsing for authorized key files)
        "tomli>=2.0.0;python_version<'3.11'",
        force_build=True,
    )
    .add_local_dir("ryeos_node", remote_path="/app/ryeos_node", copy=True)
    .env({
        "CAS_BASE_PATH": "/cas",
        "PYTHONPATH": "/app",
        "RYE_KERNEL_PYTHON": "/usr/local/bin/python3",
        "REGISTRY_ENABLED": "true",
    })
)


@app.function(
    image=image,
    volumes={"/cas": cas_volume},
    timeout=300,
    min_containers=1,
)
@modal.concurrent(max_inputs=8)
@modal.asgi_app()
def node():
    """ryeos-node — CAS-native execution server."""
    from ryeos_node.init import ensure_node_space

    ensure_node_space("/cas")
    cas_volume.commit()

    from ryeos_node.server import app as fastapi_app

    return fastapi_app


@app.function(
    image=image,
    volumes={"/cas": cas_volume},
    timeout=60,
)
def authorize_key(
    fingerprint: str,
    public_key: str,
    owner: str = "ci",
):
    """Authorize a signing key on the node volume.

    Usage:
      modal run modal_app.py::authorize_key \
        --fingerprint 4b987fd4e40303ac \
        --public-key 'ed25519:LS0t...' \
        --owner github-ci
    """
    import hashlib
    import time
    from pathlib import Path
    from ryeos_node.init import ensure_node_space
    from rye.primitives.signing import load_keypair, compute_key_fingerprint, sign_hash

    ensure_node_space("/cas")

    caps = ["rye.registry.*", "rye.objects.*"]
    fp = fingerprint.removeprefix("fp:")

    # Accept ed25519: prefix or raw base64
    pub_b64 = public_key.removeprefix("ed25519:")

    # Build TOML body
    caps_toml = ", ".join(f'"{c}"' for c in caps)
    body = (
        f'fingerprint = "{fp}"\n'
        f'owner = "{owner}"\n'
        f'public_key = "ed25519:{pub_b64}"\n'
        f'capabilities = [{caps_toml}]\n'
    )

    # Sign with node's key
    node_priv, node_pub = load_keypair(Path("/cas/signing"))
    node_fp = compute_key_fingerprint(node_pub)
    content_hash = hashlib.sha256(body.encode()).hexdigest()
    sig_b64 = sign_hash(content_hash, node_priv)
    timestamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

    signed = f"# rye:signed:{timestamp}:{content_hash}:{sig_b64}:{node_fp}\n{body}"

    key_file = Path("/cas/config/authorized_keys") / f"{fp}.toml"
    key_file.parent.mkdir(parents=True, exist_ok=True)
    key_file.write_text(signed, encoding="utf-8")

    cas_volume.commit()

    result = {
        "authorized": f"fp:{fp}",
        "owner": owner,
        "capabilities": caps,
        "signed_by": f"fp:{node_fp}",
        "key_file": str(key_file),
    }
    print(result)
    return result
