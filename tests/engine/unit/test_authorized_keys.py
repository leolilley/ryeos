"""Tests for the shared authorized-key document helper."""

import pytest

from rye.utils.authorized_keys import (
    build_authorized_key_body,
    build_and_sign_authorized_key,
    parse_authorized_key,
    sign_authorized_key,
    validate_fingerprint,
    validate_label,
    validate_scopes,
    verify_authorized_key_signature,
)


class TestValidation:
    """Test input validation functions."""

    def test_valid_fingerprint(self):
        validate_fingerprint("4b987fd4e40303ac")

    def test_fingerprint_too_short(self):
        with pytest.raises(ValueError, match="16 lowercase hex"):
            validate_fingerprint("4b987fd4")

    def test_fingerprint_uppercase(self):
        with pytest.raises(ValueError, match="16 lowercase hex"):
            validate_fingerprint("4B987FD4E40303AC")

    def test_fingerprint_path_traversal(self):
        with pytest.raises(ValueError):
            validate_fingerprint("../../etc/passwd")

    def test_valid_label(self):
        validate_label("my-key-2026")

    def test_label_with_quotes(self):
        with pytest.raises(ValueError, match="quotes"):
            validate_label('bad"label')

    def test_label_with_newline(self):
        with pytest.raises(ValueError):
            validate_label("bad\nlabel")

    def test_label_too_long(self):
        with pytest.raises(ValueError, match="128"):
            validate_label("x" * 200)

    def test_valid_scopes(self):
        validate_scopes(["*", "remote:execute", "registry:read"])

    def test_scope_with_quotes(self):
        with pytest.raises(ValueError):
            validate_scopes(['bad"scope'])

    def test_scope_too_long(self):
        with pytest.raises(ValueError, match="256"):
            validate_scopes(["x" * 300])


class TestBuildAndSign:
    """Test document building and signing."""

    def test_build_body(self):
        body, ts = build_authorized_key_body(
            fingerprint="4b987fd4e40303ac",
            public_key_encoded="ed25519:AAAA",
            label="test",
            scopes=["*"],
        )
        assert 'fingerprint = "4b987fd4e40303ac"' in body
        assert 'public_key = "ed25519:AAAA"' in body
        assert 'label = "test"' in body
        assert 'scopes = ["*"]' in body
        assert 'created_at = "' in body

    def test_build_body_extra_fields(self):
        body, _ = build_authorized_key_body(
            fingerprint="4b987fd4e40303ac",
            public_key_encoded="ed25519:AAAA",
            extra_fields={"created_via": "bootstrap_env"},
        )
        assert 'created_via = "bootstrap_env"' in body

    def test_build_and_sign_roundtrip(self):
        from rye.primitives.signing import generate_keypair, compute_key_fingerprint

        node_priv, node_pub = generate_keypair()
        user_priv, user_pub = generate_keypair()

        signed_doc, fp = build_and_sign_authorized_key(
            public_key_pem=user_pub,
            signer_private=node_priv,
            signer_public=node_pub,
            label="roundtrip-test",
        )

        assert signed_doc.startswith("# rye:signed:")
        assert fp == compute_key_fingerprint(user_pub)

        # Verify signature
        parsed = verify_authorized_key_signature(signed_doc, node_pub)
        assert parsed["fingerprint"] == fp
        assert parsed["label"] == "roundtrip-test"

    def test_verify_rejects_tampered(self):
        from rye.primitives.signing import generate_keypair

        node_priv, node_pub = generate_keypair()
        user_priv, user_pub = generate_keypair()

        signed_doc, _ = build_and_sign_authorized_key(
            public_key_pem=user_pub,
            signer_private=node_priv,
            signer_public=node_pub,
        )

        # Tamper with the body
        tampered = signed_doc.replace("unnamed", "HACKED")
        with pytest.raises(ValueError, match="hash mismatch"):
            verify_authorized_key_signature(tampered, node_pub)

    def test_verify_rejects_wrong_signer(self):
        from rye.primitives.signing import generate_keypair

        node_priv, node_pub = generate_keypair()
        other_priv, other_pub = generate_keypair()
        user_priv, user_pub = generate_keypair()

        signed_doc, _ = build_and_sign_authorized_key(
            public_key_pem=user_pub,
            signer_private=node_priv,
            signer_public=node_pub,
        )

        with pytest.raises(ValueError, match="Wrong signer"):
            verify_authorized_key_signature(signed_doc, other_pub)

    def test_parse_without_verification(self):
        doc = '# rye:signed:TS:HASH:SIG:FP\nfingerprint = "4b987fd4e40303ac"\nlabel = "test"\n'
        parsed = parse_authorized_key(doc)
        assert parsed["fingerprint"] == "4b987fd4e40303ac"


class TestBuildBodyValidation:
    """Test that build_authorized_key_body validates inputs."""

    def test_rejects_bad_fingerprint(self):
        with pytest.raises(ValueError):
            build_authorized_key_body(
                fingerprint="bad",
                public_key_encoded="ed25519:AAAA",
            )

    def test_rejects_bad_label(self):
        with pytest.raises(ValueError):
            build_authorized_key_body(
                fingerprint="4b987fd4e40303ac",
                public_key_encoded="ed25519:AAAA",
                label='bad"label',
            )

    def test_rejects_bad_scopes(self):
        with pytest.raises(ValueError):
            build_authorized_key_body(
                fingerprint="4b987fd4e40303ac",
                public_key_encoded="ed25519:AAAA",
                scopes=['bad"scope'],
            )

    def test_rejects_bad_extra_fields_value(self):
        with pytest.raises(ValueError, match="extra_fields"):
            build_authorized_key_body(
                fingerprint="4b987fd4e40303ac",
                public_key_encoded="ed25519:AAAA",
                extra_fields={"created_via": 'bad"value'},
            )

    def test_rejects_bad_extra_fields_key(self):
        with pytest.raises(ValueError, match="extra_fields"):
            build_authorized_key_body(
                fingerprint="4b987fd4e40303ac",
                public_key_encoded="ed25519:AAAA",
                extra_fields={"bad\nkey": "value"},
            )
