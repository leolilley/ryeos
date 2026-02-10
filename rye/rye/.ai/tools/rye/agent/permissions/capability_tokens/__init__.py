# PROTECTED: Core RYE tool - do not override
"""RYE capabilities - capability token system for permission enforcement."""

from .capability_tokens import (
    CapabilityToken,
    generate_keypair,
    save_keypair,
    load_private_key,
    load_public_key,
    ensure_keypair,
    sign_token,
    verify_token,
    mint_token,
    attenuate_token,
    expand_capabilities,
    check_capability,
    check_all_capabilities,
    permissions_to_caps,
    PERMISSION_TO_CAPABILITY,
    CAPABILITY_HIERARCHY,
)

__all__ = [
    "CapabilityToken",
    "generate_keypair",
    "save_keypair",
    "load_private_key",
    "load_public_key",
    "ensure_keypair",
    "sign_token",
    "verify_token",
    "mint_token",
    "attenuate_token",
    "expand_capabilities",
    "check_capability",
    "check_all_capabilities",
    "permissions_to_caps",
    "PERMISSION_TO_CAPABILITY",
    "CAPABILITY_HIERARCHY",
]
