"""Tests for Phase 1 identity/auth crypto primitives."""

import importlib.util
import json
import sys
import time
from pathlib import Path

import pytest

from rye.primitives.signing import (
    compute_box_fingerprint,
    compute_key_fingerprint,
    ensure_full_keypair,
    generate_full_keypair,
    generate_keypair,
    load_box_keypair,
    save_box_keypair,
    save_keypair,
)

PROJECT_ROOT = Path(__file__).parent.parent.parent.parent


def _import_bundle_module(rel_path: str, module_name: str):
    """Import a module from the core bundle tools."""
    full_path = (
        PROJECT_ROOT
        / "ryeos"
        / "bundles"
        / "core"
        / "ryeos_core"
        / ".ai"
        / "tools"
        / rel_path
    )
    spec = importlib.util.spec_from_file_location(module_name, full_path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = mod
    spec.loader.exec_module(mod)
    return mod


sign_object_mod = _import_bundle_module(
    "rye/core/crypto/sign_object.py", "sign_object_mod"
)
request_signing_mod = _import_bundle_module(
    "rye/core/crypto/request_signing.py", "request_signing_mod"
)


# ---------------------------------------------------------------------------
# 1. signing.py — X25519 / full keypair functions
# ---------------------------------------------------------------------------


class TestGenerateFullKeypair:
    def test_generate_full_keypair(self):
        result = generate_full_keypair()
        assert len(result) == 4
        private_pem, public_pem, box_key, box_pub = result

        # All items are bytes
        for item in result:
            assert isinstance(item, bytes)

        # Ed25519 PEM headers
        assert private_pem.startswith(b"-----BEGIN PRIVATE KEY-----")
        assert public_pem.startswith(b"-----BEGIN PUBLIC KEY-----")

        # Box keys are non-empty
        assert len(box_key) > 0
        assert len(box_pub) > 0


class TestSaveLoadBoxKeypair:
    def test_save_load_box_keypair(self, tmp_path):
        _, _, box_key, box_pub = generate_full_keypair()

        save_box_keypair(box_key, box_pub, tmp_path)
        loaded_key, loaded_pub = load_box_keypair(tmp_path)

        assert loaded_key == box_key
        assert loaded_pub == box_pub


class TestComputeBoxFingerprint:
    def test_compute_box_fingerprint(self):
        _, _, _, box_pub = generate_full_keypair()
        fp = compute_box_fingerprint(box_pub)

        assert isinstance(fp, str)
        assert len(fp) == 16
        # All hex characters
        int(fp, 16)


class TestSaveKeypairWithBoxKeys:
    def test_save_keypair_with_box_keys(self, tmp_path):
        private_pem, public_pem, box_key, box_pub = generate_full_keypair()

        save_keypair(
            private_pem, public_pem, tmp_path, box_key=box_key, box_pub=box_pub
        )

        assert (tmp_path / "private_key.pem").exists()
        assert (tmp_path / "public_key.pem").exists()
        assert (tmp_path / "box_key.pem").exists()
        assert (tmp_path / "box_pub.pem").exists()

        assert (tmp_path / "private_key.pem").read_bytes() == private_pem
        assert (tmp_path / "public_key.pem").read_bytes() == public_pem
        assert (tmp_path / "box_key.pem").read_bytes() == box_key
        assert (tmp_path / "box_pub.pem").read_bytes() == box_pub


class TestEnsureFullKeypair:
    def test_ensure_full_keypair(self, tmp_path):
        key_dir = tmp_path / "keys"

        # First call generates keys
        result = ensure_full_keypair(key_dir)
        assert len(result) == 4
        private_pem, public_pem, box_key, box_pub = result
        for item in result:
            assert isinstance(item, bytes)
            assert len(item) > 0

        # Second call returns existing keys
        result2 = ensure_full_keypair(key_dir)
        assert result2 == result


# ---------------------------------------------------------------------------
# 2. sign_object.py
# ---------------------------------------------------------------------------


class TestCanonicalJson:
    def test_canonical_json(self):
        data = {"z": 1, "a": 2, "_signature": {"sig": "xxx"}, "m": 3}
        result = sign_object_mod.canonical_json(data)
        parsed = json.loads(result)

        # _signature excluded
        assert "_signature" not in parsed

        # Sorted keys
        assert list(parsed.keys()) == ["a", "m", "z"]

        # Compact separators (no spaces)
        assert " " not in result


class TestSignObject:
    def test_sign_object(self):
        private_pem, public_pem = generate_keypair()
        data = {"kind": "test/v1", "name": "hello"}

        signed = sign_object_mod.sign_object(data, private_pem, public_pem)

        assert "_signature" in signed
        sig_block = signed["_signature"]
        assert "signer" in sig_block
        assert sig_block["signer"].startswith("fp:")
        assert "sig" in sig_block
        assert "signed_at" in sig_block


class TestVerifyObject:
    def test_verify_object(self):
        private_pem, public_pem = generate_keypair()
        data = {"kind": "test/v1", "value": 42}

        signed = sign_object_mod.sign_object(data, private_pem, public_pem)
        assert sign_object_mod.verify_object(signed, public_pem) is True

    def test_verify_object_wrong_key(self):
        priv1, pub1 = generate_keypair()
        _, pub2 = generate_keypair()
        data = {"kind": "test/v1", "value": 42}

        signed = sign_object_mod.sign_object(data, priv1, pub1)
        assert sign_object_mod.verify_object(signed, pub2) is False

    def test_verify_object_tampered(self):
        private_pem, public_pem = generate_keypair()
        data = {"kind": "test/v1", "value": 42}

        signed = sign_object_mod.sign_object(data, private_pem, public_pem)
        signed["value"] = 999
        assert sign_object_mod.verify_object(signed, public_pem) is False


class TestSignObjectWithKeyDir:
    def test_sign_object_with_key_dir(self, tmp_path):
        private_pem, public_pem = generate_keypair()
        save_keypair(private_pem, public_pem, tmp_path)

        data = {"kind": "test/v1", "field": "abc"}
        signed = sign_object_mod.sign_object_with_key_dir(data, tmp_path)

        assert "_signature" in signed
        assert sign_object_mod.verify_object(signed, public_pem) is True


# ---------------------------------------------------------------------------
# 3. request_signing.py
# ---------------------------------------------------------------------------


class TestCanonicalPath:
    def test_canonical_path_simple(self):
        assert request_signing_mod.canonical_path("/foo/bar") == "/foo/bar"

    def test_canonical_path_with_query(self):
        result = request_signing_mod.canonical_path("/foo?b=2&a=1")
        assert result == "/foo?a=1&b=2"

    def test_canonical_path_full_url(self):
        result = request_signing_mod.canonical_path("https://example.com/foo?b=2&a=1")
        assert result == "/foo?a=1&b=2"


class TestSignRequest:
    def test_sign_request(self):
        private_pem, public_pem = generate_keypair()
        headers = request_signing_mod.sign_request(
            method="POST",
            url_or_path="/api/v1/test",
            body=b'{"hello": "world"}',
            audience="fp:abc123",
            private_key_pem=private_pem,
            public_key_pem=public_pem,
        )

        assert "X-Rye-Key-Id" in headers
        assert headers["X-Rye-Key-Id"].startswith("fp:")
        assert "X-Rye-Timestamp" in headers
        assert headers["X-Rye-Timestamp"].isdigit()
        assert "X-Rye-Nonce" in headers
        assert len(headers["X-Rye-Nonce"]) == 32  # 16 bytes hex
        assert "X-Rye-Signature" in headers


class TestVerifyRequestSignature:
    def test_verify_request_signature(self):
        private_pem, public_pem = generate_keypair()
        fp = compute_key_fingerprint(public_pem)
        audience = f"fp:{fp}"

        headers = request_signing_mod.sign_request(
            method="GET",
            url_or_path="/api/v1/check",
            body=None,
            audience=audience,
            private_key_pem=private_pem,
            public_key_pem=public_pem,
        )

        result = request_signing_mod.verify_request_signature(
            method="GET",
            url_or_path="/api/v1/check",
            body=None,
            audience=audience,
            headers=headers,
            public_key_pem=public_pem,
        )
        assert result is True

    def test_verify_request_signature_wrong_key(self):
        priv1, pub1 = generate_keypair()
        _, pub2 = generate_keypair()
        audience = "fp:test"

        headers = request_signing_mod.sign_request(
            method="POST",
            url_or_path="/api/v1/data",
            body=b"payload",
            audience=audience,
            private_key_pem=priv1,
            public_key_pem=pub1,
        )

        result = request_signing_mod.verify_request_signature(
            method="POST",
            url_or_path="/api/v1/data",
            body=b"payload",
            audience=audience,
            headers=headers,
            public_key_pem=pub2,
        )
        assert result is False

    def test_verify_request_signature_expired(self, monkeypatch):
        private_pem, public_pem = generate_keypair()
        audience = "fp:test"

        real_time = time.time

        # Sign at current time
        headers = request_signing_mod.sign_request(
            method="GET",
            url_or_path="/api/v1/check",
            body=None,
            audience=audience,
            private_key_pem=private_pem,
            public_key_pem=public_pem,
        )

        # Advance time by 600 seconds for verification
        monkeypatch.setattr(time, "time", lambda: real_time() + 600)

        result = request_signing_mod.verify_request_signature(
            method="GET",
            url_or_path="/api/v1/check",
            body=None,
            audience=audience,
            headers=headers,
            public_key_pem=public_pem,
            max_age_seconds=0,
        )
        assert result is False
