"""Tests for lillux identity envelope open — Rust-side sealed envelope decryption."""

import base64
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest

LILLUX = shutil.which("lillux")

pytestmark = pytest.mark.skipif(
    LILLUX is None,
    reason="lillux binary not found on PATH",
)


def _lillux(*args, stdin_data=None):
    """Run lillux binary, return parsed JSON output."""
    result = subprocess.run(
        [LILLUX, *args],
        input=stdin_data,
        capture_output=True,
        text=True,
        timeout=10,
    )
    return json.loads(result.stdout), result.returncode


def _generate_keypair(tmp_path):
    """Generate a keypair via lillux, return key_dir path."""
    key_dir = str(tmp_path / "keys")
    output, _ = _lillux("identity", "keypair", "generate", "--key-dir", key_dir)
    assert "fingerprint" in output
    assert "box_pub" in output
    return key_dir, output["box_pub"]


def _seal_secrets(env_map, box_pub_b64):
    """Seal secrets using the Python envelope module (client-side)."""
    # Import the core bundle's envelope sealing code
    from ryeos_core_path import get_envelope_mod
    mod = get_envelope_mod()
    return mod.seal_secrets(env_map, box_pub_b64.encode())


@pytest.fixture
def keypair(tmp_path):
    """Generate a fresh keypair, return (key_dir, box_pub_b64)."""
    return _generate_keypair(tmp_path)


# ---------------------------------------------------------------------------
# Helpers to load the sealing module
# ---------------------------------------------------------------------------

class ryeos_core_path:
    @staticmethod
    def get_envelope_mod():
        import importlib.util
        # Find the core bundle envelope.py
        import ryeos_core
        core_root = Path(ryeos_core.__file__).parent
        envelope_path = core_root / ".ai" / "tools" / "rye" / "core" / "crypto" / "envelope.py"
        spec = importlib.util.spec_from_file_location("envelope", envelope_path)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod


def _seal(env_map, box_pub_b64):
    """Seal an env map to a box public key."""
    mod = ryeos_core_path.get_envelope_mod()
    return mod.seal_secrets(env_map, box_pub_b64.encode())


# ---------------------------------------------------------------------------
# Round-trip tests
# ---------------------------------------------------------------------------


class TestEnvelopeOpen:
    """Tests for lillux identity envelope open subcommand."""

    def test_round_trip(self, keypair):
        """Seal in Python, open in Rust — basic round-trip."""
        key_dir, box_pub = keypair
        env_map = {"BACKEND_API_URL": "https://example.com", "ZEN_API_KEY": "sk-test-123"}
        sealed = _seal(env_map, box_pub)

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed),
        )
        assert code == 0
        assert result["env"] == env_map

    def test_empty_env_map(self, keypair):
        """Empty env map round-trips correctly."""
        key_dir, box_pub = keypair
        sealed = _seal({}, box_pub)

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed),
        )
        assert code == 0
        assert result["env"] == {}

    def test_large_value(self, keypair):
        """Values up to 64KB are accepted."""
        key_dir, box_pub = keypair
        env_map = {"BIG_VALUE": "x" * (64 * 1024)}
        sealed = _seal(env_map, box_pub)

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed),
        )
        assert code == 0
        assert result["env"]["BIG_VALUE"] == env_map["BIG_VALUE"]


# ---------------------------------------------------------------------------
# Safety filter tests
# ---------------------------------------------------------------------------


class TestEnvelopeSafetyFilter:
    """Tests for env name safety filtering in Rust."""

    def test_reserved_names_filtered(self, keypair):
        """Reserved env names are filtered out."""
        key_dir, box_pub = keypair
        env_map = {
            "GOOD_VAR": "safe",
            "PATH": "filtered",
            "HOME": "filtered",
            "PYTHONPATH": "filtered",
            "LD_PRELOAD": "filtered",
        }
        sealed = _seal(env_map, box_pub)

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed),
        )
        assert code == 0
        assert result["env"] == {"GOOD_VAR": "safe"}
        assert set(result["skipped"]) == {"PATH", "HOME", "PYTHONPATH", "LD_PRELOAD"}

    def test_reserved_prefixes_filtered(self, keypair):
        """Reserved prefix names are filtered out."""
        key_dir, box_pub = keypair
        env_map = {
            "MY_KEY": "safe",
            "SUPABASE_URL": "filtered",
            "AWS_SECRET": "filtered",
            "GOOGLE_API_KEY": "filtered",
            "GITHUB_TOKEN": "filtered",
            "DOCKER_HOST": "filtered",
        }
        sealed = _seal(env_map, box_pub)

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed),
        )
        assert code == 0
        assert result["env"] == {"MY_KEY": "safe"}
        assert "SUPABASE_URL" in result["skipped"]
        assert "AWS_SECRET" in result["skipped"]

    def test_invalid_identifier_filtered(self, keypair):
        """Non-identifier names are filtered."""
        key_dir, box_pub = keypair
        env_map = {
            "VALID_NAME": "safe",
            "123invalid": "filtered",
            "has space": "filtered",
        }
        sealed = _seal(env_map, box_pub)

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed),
        )
        assert code == 0
        assert result["env"] == {"VALID_NAME": "safe"}


# ---------------------------------------------------------------------------
# Error handling tests
# ---------------------------------------------------------------------------


class TestEnvelopeErrors:
    """Tests for error cases in lillux identity envelope open."""

    def test_wrong_key_fails(self, tmp_path):
        """Opening with wrong key returns error."""
        key_dir_1, box_pub_1 = _generate_keypair(tmp_path / "keys1")
        key_dir_2, _ = _generate_keypair(tmp_path / "keys2")

        sealed = _seal({"SECRET": "value"}, box_pub_1)

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir_2,
            stdin_data=json.dumps(sealed),
        )
        assert "error" in result
        assert "recipient mismatch" in result["error"]

    def test_tampered_ciphertext_fails(self, keypair):
        """Tampered ciphertext fails AEAD authentication."""
        key_dir, box_pub = keypair
        sealed = _seal({"KEY": "val"}, box_pub)

        ct = base64.urlsafe_b64decode(sealed["ciphertext"] + "==")
        tampered = bytes([ct[0] ^ 0xFF]) + ct[1:]
        sealed["ciphertext"] = base64.urlsafe_b64encode(tampered).decode().rstrip("=")

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed),
        )
        assert "error" in result
        assert "decryption failed" in result["error"]

    def test_invalid_version_fails(self, keypair):
        """Unsupported envelope version returns error."""
        key_dir, _ = keypair
        envelope = {"version": 99, "enc": "x", "ciphertext": "y", "aad_fields": {"kind": "execution-secrets/v1", "recipient": "fp:0000000000000000"}}

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(envelope),
        )
        assert "error" in result
        assert "version" in result["error"]

    def test_missing_fields_fails(self, keypair):
        """Missing required fields return error."""
        key_dir, _ = keypair
        envelope = {"version": 1}

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(envelope),
        )
        assert "error" in result
        assert "missing" in result["error"].lower() or "parse" in result["error"].lower()

    def test_wrong_kind_fails(self, keypair):
        """Wrong envelope kind returns error."""
        key_dir, _ = keypair
        envelope = {"version": 1, "enc": "x", "ciphertext": "y", "aad_fields": {"kind": "wrong/v1", "recipient": "fp:0000000000000000"}}

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(envelope),
        )
        assert "error" in result
        assert "kind" in result["error"]

    def test_missing_box_key_fails(self, tmp_path):
        """Missing box_key.pem returns error."""
        key_dir = str(tmp_path / "empty")
        Path(key_dir).mkdir()

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data='{"version":1,"enc":"x","ciphertext":"y","aad_fields":{"kind":"execution-secrets/v1","recipient":"fp:0000000000000000"}}',
        )
        assert "error" in result
        assert "box key" in result["error"]


# ---------------------------------------------------------------------------
# Validation tests
# ---------------------------------------------------------------------------


class TestEnvelopeValidation:
    """Tests for env map validation in Rust."""

    def test_nul_byte_rejected(self, keypair):
        """Values containing NUL bytes are rejected."""
        key_dir, box_pub = keypair
        env_map = {"KEY": "val\x00ue"}
        sealed = _seal(env_map, box_pub)

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed),
        )
        assert "error" in result
        assert "NUL" in result["error"]

    def test_value_too_large_rejected(self, keypair):
        """Values exceeding 64KB are rejected."""
        key_dir, box_pub = keypair
        env_map = {"HUGE": "x" * (64 * 1024 + 1)}
        sealed = _seal(env_map, box_pub)

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed),
        )
        assert "error" in result
        assert "too large" in result["error"]


# ---------------------------------------------------------------------------
# Rust seal tests
# ---------------------------------------------------------------------------


class TestEnvelopeSeal:
    """Tests for lillux identity envelope seal."""

    def test_seal_round_trip(self, keypair):
        """Seal in Rust, open in Rust — full round-trip."""
        key_dir, box_pub = keypair
        env_map = {"MY_SECRET": "hunter2", "API_KEY": "sk-test-123"}

        # Seal via CLI
        sealed_result, code = _lillux(
            "identity", "envelope", "seal",
            "--box-pub", os.path.join(key_dir, "box_pub.pem"),
            stdin_data=json.dumps(env_map),
        )
        assert "error" not in sealed_result
        assert "version" in sealed_result
        assert "ciphertext" in sealed_result

        # Open via CLI
        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed_result),
        )
        assert result["env"] == env_map

    def test_seal_with_inline_key(self, keypair):
        """Seal using --box-pub-inline flag."""
        key_dir, box_pub = keypair
        env_map = {"SECRET": "value"}

        sealed_result, code = _lillux(
            "identity", "envelope", "seal",
            "--box-pub-inline", box_pub,
            stdin_data=json.dumps(env_map),
        )
        assert "error" not in sealed_result

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed_result),
        )
        assert result["env"] == env_map

    def test_seal_rejects_unsafe_names(self, keypair):
        """Seal rejects reserved env names."""
        key_dir, box_pub = keypair
        env_map = {"PATH": "/usr/bin", "GOOD_VAR": "safe"}

        result, code = _lillux(
            "identity", "envelope", "seal",
            "--box-pub", os.path.join(key_dir, "box_pub.pem"),
            stdin_data=json.dumps(env_map),
        )
        assert "error" in result
        assert "unsafe" in result["error"].lower() or "refusing" in result["error"].lower()

    def test_seal_rejects_reserved_prefix(self, keypair):
        """Seal rejects reserved prefix names."""
        key_dir, box_pub = keypair
        env_map = {"AWS_SECRET_KEY": "xxx"}

        result, code = _lillux(
            "identity", "envelope", "seal",
            "--box-pub", os.path.join(key_dir, "box_pub.pem"),
            stdin_data=json.dumps(env_map),
        )
        assert "error" in result

    def test_seal_rejects_non_string_values(self, keypair):
        """Seal rejects non-string values in env map."""
        key_dir, box_pub = keypair

        result, code = _lillux(
            "identity", "envelope", "seal",
            "--box-pub", os.path.join(key_dir, "box_pub.pem"),
            stdin_data='{"KEY": 123}',
        )
        assert "error" in result
        assert "string" in result["error"].lower()

    def test_seal_empty_map(self, keypair):
        """Seal accepts empty env map."""
        key_dir, box_pub = keypair

        sealed_result, code = _lillux(
            "identity", "envelope", "seal",
            "--box-pub", os.path.join(key_dir, "box_pub.pem"),
            stdin_data="{}",
        )
        assert "error" not in sealed_result

        result, code = _lillux(
            "identity", "envelope", "open", "--key-dir", key_dir,
            stdin_data=json.dumps(sealed_result),
        )
        assert result["env"] == {}


# ---------------------------------------------------------------------------
# Rust seal → Python open interop tests
# ---------------------------------------------------------------------------


class TestEnvelopeSealPythonInterop:
    """Tests for Rust seal → Python open interoperability."""

    def test_rust_seal_python_open(self, keypair):
        """Envelopes sealed by Rust can be opened by Python."""
        key_dir, box_pub = keypair
        env_map = {"INTEROP_KEY": "rust-to-python", "ANOTHER": "value"}

        # Seal via Rust CLI
        sealed_result, code = _lillux(
            "identity", "envelope", "seal",
            "--box-pub", os.path.join(key_dir, "box_pub.pem"),
            stdin_data=json.dumps(env_map),
        )
        assert "error" not in sealed_result

        # Open via Python
        mod = ryeos_core_path.get_envelope_mod()
        box_key = Path(key_dir, "box_key.pem").read_text().strip().encode()
        decrypted = mod.open_envelope(sealed_result, box_key)
        assert decrypted == env_map


# ---------------------------------------------------------------------------
# Validate command tests
# ---------------------------------------------------------------------------


class TestEnvelopeValidate:
    """Tests for lillux identity envelope validate."""

    def test_valid_env_map(self):
        """Valid env map returns valid=true."""
        result, _ = _lillux(
            "identity", "envelope", "validate",
            stdin_data='{"MY_KEY": "value", "OTHER": "data"}',
        )
        assert result["valid"] is True
        assert result["errors"] == []
        assert result["unsafe_names"] == []

    def test_unsafe_names_reported(self):
        """Unsafe names are reported."""
        result, _ = _lillux(
            "identity", "envelope", "validate",
            stdin_data='{"GOOD": "ok", "PATH": "bad", "AWS_KEY": "bad"}',
        )
        assert result["valid"] is False
        assert "PATH" in result["unsafe_names"]
        assert "AWS_KEY" in result["unsafe_names"]
        assert result["count"] == 3

    def test_nul_byte_in_value(self):
        """NUL bytes in values cause errors."""
        result, _ = _lillux(
            "identity", "envelope", "validate",
            stdin_data=json.dumps({"KEY": "val\x00ue"}),
        )
        assert result["valid"] is False
        assert any("NUL" in e for e in result["errors"])


# ---------------------------------------------------------------------------
# Inspect command tests
# ---------------------------------------------------------------------------


class TestEnvelopeInspect:
    """Tests for lillux identity envelope inspect."""

    def test_inspect_valid_envelope(self, keypair):
        """Inspect a well-formed sealed envelope."""
        key_dir, box_pub = keypair
        sealed = _seal({"KEY": "val"}, box_pub)

        result, _ = _lillux(
            "identity", "envelope", "inspect",
            stdin_data=json.dumps(sealed),
        )
        assert result["well_formed"] is True
        assert result["version"] == 1
        assert result["declared_kind"] == "execution-secrets/v1"
        assert result["declared_recipient"] is not None
        assert result["enc_bytes"] == 32
        assert result["ciphertext_bytes"] > 0
        assert result["warnings"] == []

    def test_inspect_missing_recipient(self):
        """Inspect flags missing aad_fields.recipient."""
        envelope = {
            "version": 1, "enc": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "ciphertext": "AAAA",
            "aad_fields": {"kind": "execution-secrets/v1"}
        }
        result, _ = _lillux(
            "identity", "envelope", "inspect",
            stdin_data=json.dumps(envelope),
        )
        assert result["well_formed"] is False
        assert any("recipient" in w.lower() for w in result["warnings"])

    def test_inspect_invalid_base64_enc(self):
        """Inspect flags invalid base64 in enc field."""
        envelope = {
            "version": 1, "enc": "!!!not-base64!!!",
            "ciphertext": "AAAA",
            "aad_fields": {"kind": "execution-secrets/v1", "recipient": "fp:abcd"}
        }
        result, _ = _lillux(
            "identity", "envelope", "inspect",
            stdin_data=json.dumps(envelope),
        )
        assert result["well_formed"] is False

    def test_inspect_recipient_mismatch(self, keypair):
        """Inspect warns when top-level recipient differs from aad_fields.recipient."""
        key_dir, box_pub = keypair
        sealed = _seal({"KEY": "val"}, box_pub)
        # Tamper top-level recipient
        sealed["recipient"] = "fp:0000000000000000"

        result, _ = _lillux(
            "identity", "envelope", "inspect",
            stdin_data=json.dumps(sealed),
        )
        assert any("mismatch" in w.lower() for w in result["warnings"])
