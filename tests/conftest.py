"""Root conftest: Python path setup for all tests."""

import os
import sys
from pathlib import Path

# Set up PROJECT_ROOT for all test modules (exported for use in test files)
PROJECT_ROOT = Path(__file__).parent.parent

# Auto-source venv if it exists (add site-packages and process .pth files)
_VENV_ROOT = PROJECT_ROOT / ".venv"
if _VENV_ROOT.exists():
    _SITE_PACKAGES = list(_VENV_ROOT.glob("lib/python*/site-packages"))
    if _SITE_PACKAGES:
        _SITE_PKG = str(_SITE_PACKAGES[0])
        if _SITE_PKG not in sys.path:
            sys.path.insert(0, _SITE_PKG)
        # Process .pth files so editable installs are found
        import site
        site.addsitedir(_SITE_PKG)
    # Add venv bin/ to PATH so venv-installed binaries (e.g. lillux) are found
    _VENV_BIN = str(_VENV_ROOT / "bin")
    if _VENV_BIN not in os.environ.get("PATH", ""):
        os.environ["PATH"] = _VENV_BIN + os.pathsep + os.environ.get("PATH", "")

# Add service packages to sys.path so tests can import them
_SERVICES_DIR = PROJECT_ROOT / "services"
if _SERVICES_DIR.exists():
    for _svc in _SERVICES_DIR.iterdir():
        if _svc.is_dir() and str(_svc) not in sys.path:
            sys.path.insert(0, str(_svc))

# Add core runtime lib to sys.path so importlib.util.spec_from_file_location works
_MODULE_LOADER_DIR = (
    PROJECT_ROOT
    / "ryeos" / "bundles" / "core" / "ryeos_core" / ".ai" / "tools" / "rye" / "core" / "runtimes" / "python" / "lib"
)
if str(_MODULE_LOADER_DIR) not in sys.path:
    sys.path.insert(0, str(_MODULE_LOADER_DIR))


# Pre-import core runtime lib modules so tool re-exports don't circular-import.
import importlib.util as _ilu
for _mod_name in ("condition_evaluator", "interpolation", "module_loader"):
    if _mod_name not in sys.modules:
        _mod_path = _MODULE_LOADER_DIR / f"{_mod_name}.py"
        if _mod_path.exists():
            _spec = _ilu.spec_from_file_location(_mod_name, _mod_path)
            _mod = _ilu.module_from_spec(_spec)
            sys.modules[_mod_name] = _mod
            _spec.loader.exec_module(_mod)


def get_bundle_path(bundle_type: str, relative_path: str) -> Path:
    """Get absolute path to a bundle tool/directive file.
    
    Args:
        bundle_type: 'core' or 'standard'
        relative_path: Path relative to .ai/, e.g. 'tools/rye/core/parsers/markdown/xml.py'
    
    Returns:
        Absolute Path object
    """
    bundle_names = {
        'core': 'ryeos_core',
        'standard': 'ryeos_std',
    }
    bundle_name = bundle_names[bundle_type]
    return PROJECT_ROOT / "ryeos" / "bundles" / bundle_type / bundle_name / ".ai" / relative_path


# ── Shared fixtures ──────────────────────────────────────────────────────────

import pytest

from rye.primitives.signing import (
    generate_keypair,
    save_keypair,
    compute_key_fingerprint,
)


def get_env_signing_pubkey() -> bytes | None:
    """Read the real signing public key from the current (un-monkeypatched) user space.

    Returns the PEM bytes of whatever key signed bundle items on disk.
    In CI this is the CI key; locally it's the dev's personal key.
    Returns None if no signing key is configured.
    """
    from rye.utils.path_utils import get_user_space
    from rye.constants import AI_DIR

    pubkey_path = get_user_space() / AI_DIR / "config" / "keys" / "signing" / "public_key.pem"
    if pubkey_path.is_file():
        return pubkey_path.read_bytes()
    return None


@pytest.fixture
def _setup_user_space(tmp_path, monkeypatch):
    """Set up a temporary USER_SPACE with Ed25519 keys and trust store for all tests."""
    # Capture BEFORE monkeypatching USER_SPACE.
    real_signing_pubkey = get_env_signing_pubkey()

    user_space = tmp_path / "user_space"
    user_space.mkdir()

    monkeypatch.setenv("USER_SPACE", str(user_space))

    from rye.utils.signature_formats import clear_signature_formats_cache
    from rye.constants import AI_DIR
    clear_signature_formats_cache()

    # Generate and save keypair for signing (in ~/.ai/config/keys/signing)
    signing_key_dir = user_space / AI_DIR / "config" / "keys" / "signing"
    private_pem, public_pem = generate_keypair()
    save_keypair(private_pem, public_pem, signing_key_dir)

    # Also generate a key for general use
    key_dir = user_space / AI_DIR / "keys"
    private_pem_general, public_pem_general = generate_keypair()
    save_keypair(private_pem_general, public_pem_general, key_dir)

    # Add both public keys to user space trust store
    from rye.utils.trust_store import TrustStore
    store = TrustStore(project_path=user_space)
    store.add_key(public_pem, owner="local", space="user", version="1.0.0")
    store.add_key(public_pem_general, owner="local", space="user", version="1.0.0")

    # Trust the real signing key so bundle items signed on disk are verified.
    # In CI: this is the CI key (also in bundle trust stores).
    # Locally: this is the dev's personal key.
    if real_signing_pubkey:
        store.add_key(real_signing_pubkey, owner="env-signer", space="user", version="1.0.0")

    yield user_space
