"""
ryeos-remote — Modal Deployment

CAS-native remote execution server on Modal.
Volume-backed CAS storage, persistent signing key, per-user isolation.

Usage:
  modal deploy modal_app.py          # Deploy to Modal
  modal serve modal_app.py           # Local dev with Modal

Secrets required (Modal dashboard → Secrets):
  ryeos-remote:
    - SUPABASE_URL, SUPABASE_SERVICE_KEY, SUPABASE_JWT_SECRET
"""

import modal

app = modal.App("ryeos-remote")

cas_volume = modal.Volume.from_name("ryeos-remote-cas", create_if_missing=True)

image = (
    modal.Image.debian_slim(python_version="3.12")
    .pip_install(
        # Server
        "fastapi>=0.109.0",
        "uvicorn[standard]>=0.27.0",
        "pydantic>=2.5.0",
        "pydantic-settings>=2.1.0",
        # Auth
        "supabase>=2.3.0",
        "python-jose[cryptography]>=3.3.0",
        # Core engine
        "cryptography>=41.0",
        "httpx>=0.26.0",
        "pyyaml>=6.0",
        "packaging>=21.0",
    )
    .add_local_dir("ryeos_remote", remote_path="/app/ryeos_remote", copy=True)
    .add_local_dir(
        "../../lillux/kernel/lillux", remote_path="/app/lillux", copy=True,
    )
    .add_local_dir("../../ryeos/rye", remote_path="/app/rye", copy=True)
    .env({
        "CAS_BASE_PATH": "/cas",
        "SIGNING_KEY_DIR": "/cas/signing",
        "PYTHONPATH": "/app",
    })
)


@app.function(
    image=image,
    secrets=[modal.Secret.from_name("ryeos-remote")],
    volumes={"/cas": cas_volume},
    timeout=300,
)
@modal.concurrent(max_inputs=1)
@modal.asgi_app()
def remote_server():
    """ryeos-remote — CAS-native execution server."""
    from ryeos_remote.server import app as fastapi_app

    return fastapi_app
