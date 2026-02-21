"""Shared fixtures for rye tool tests with Ed25519 signing."""

import pytest

from lilux.primitives.signing import (
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

    key_dir = user_space / "keys"
    private_pem, public_pem = generate_keypair()
    save_keypair(private_pem, public_pem, key_dir)

    # Write trusted key as TOML identity document
    trust_dir = user_space / "trusted_keys"
    trust_dir.mkdir(parents=True)
    fp = compute_key_fingerprint(public_pem)

    from rye.utils.trust_store import TrustedKeyInfo
    info = TrustedKeyInfo(
        fingerprint=fp,
        owner="local",
        public_key_pem=public_pem,
    )
    (trust_dir / f"{fp}.toml").write_text(info.to_toml(), encoding="utf-8")

    yield user_space
