"""Shared fixtures for rye tool tests with Ed25519 signing."""

import sys
from pathlib import Path

# Mirror what the anchor system injects via PYTHONPATH at runtime:
# 1. Runtime lib (module_loader, condition_evaluator, etc.)
# 2. Tool anchor directories (so load_module relative imports resolve)
_PROJECT_ROOT = Path(__file__).parent.parent.parent

_MODULE_LOADER_DIR = (
    _PROJECT_ROOT
    / "ryeos" / "bundles" / "core" / "ryeos_core" / ".ai" / "tools" / "rye" / "core" / "runtimes" / "python" / "lib"
)
if str(_MODULE_LOADER_DIR) not in sys.path:
    sys.path.insert(0, str(_MODULE_LOADER_DIR))

# Pre-import core runtime lib modules so tool re-exports don't circular-import.
# At runtime the anchor system's PYTHONPATH ensures the core versions are found
# first; in tests we must seed sys.modules explicitly.
import importlib.util as _ilu
for _mod_name in ("condition_evaluator", "interpolation", "module_loader"):
    if _mod_name not in sys.modules:
        _mod_path = _MODULE_LOADER_DIR / f"{_mod_name}.py"
        if _mod_path.exists():
            _spec = _ilu.spec_from_file_location(_mod_name, _mod_path)
            _mod = _ilu.module_from_spec(_spec)
            sys.modules[_mod_name] = _mod
            _spec.loader.exec_module(_mod)

import pytest
import os

from lillux.primitives.signing import (
    generate_keypair,
    save_keypair,
    compute_key_fingerprint,
)


@pytest.fixture(autouse=True)
def _setup_user_space(tmp_path, monkeypatch):
    """Set up a temporary USER_SPACE with Ed25519 keys and trust store for all tests."""
    user_space = tmp_path / "user_space"
    user_space.mkdir()

    monkeypatch.setenv("USER_SPACE", str(user_space))

    from rye.utils.signature_formats import clear_signature_formats_cache
    clear_signature_formats_cache()

    from rye.constants import AI_DIR
    
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
    store.add_key(public_pem, owner="local", space="user")
    store.add_key(public_pem_general, owner="local", space="user")

    yield user_space
