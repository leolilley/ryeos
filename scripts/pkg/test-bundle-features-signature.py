#!/usr/bin/env python3
"""Signature gate for bundle-features.py: only the pinned publisher's signature is accepted."""

import base64
import hashlib
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "scripts/pkg/bundle-features.py"
CONFIG = ROOT / "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml"
TRUST = ROOT / "bundles/.ai/PUBLISHER_TRUST.toml"


def raw_public(key):
    return key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)


def trust_text(key):
    public = raw_public(key)
    return (f'public_key = "ed25519:{base64.b64encode(public).decode()}"\n'
            f'fingerprint = "{hashlib.sha256(public).hexdigest()}"\nowner = "test"\n')


def envelope(body, key, claimed_fingerprint=None):
    digest = hashlib.sha256(body.encode()).hexdigest()
    signature = base64.b64encode(key.sign(digest.encode())).decode()
    fingerprint = claimed_fingerprint or hashlib.sha256(raw_public(key)).hexdigest()
    return f"# ryeos:signed:2026-01-01T00:00:00Z:{digest}:{signature}:{fingerprint}\n{body}"


class SignatureGate(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.body = CONFIG.read_text().partition("\n")[2]
        self.key = Ed25519PrivateKey.generate()
        self.trust = self.dir / "trust.toml"
        self.trust.write_text(trust_text(self.key))

    def run_tool(self, text, trust=None):
        config = self.dir / "ownership.yaml"
        config.write_text(text)
        (self.dir / "bundles/.ai").mkdir(parents=True, exist_ok=True)
        (self.dir / "bundles/.ai/PUBLISHER_TRUST.toml").write_text((trust or self.trust).read_text())
        return subprocess.run([sys.executable, "-I", str(TOOL), "--repository-root", str(self.dir),
                               "--config", str(config), "--set", "core", "cargo-features", "ryeosd"],
                              capture_output=True, text=True)

    def test_valid_signature_by_pinned_key_accepted(self):
        result = self.run_tool(envelope(self.body, self.key))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_tampered_body_with_recomputed_hash_refused(self):
        tampered = self.body + "# x\n"
        _, signature, fingerprint = envelope(self.body, self.key).partition("\n")[0].rsplit(":", 2)
        digest = hashlib.sha256(tampered.encode()).hexdigest()
        forged = f"# ryeos:signed:2026-01-01T00:00:00Z:{digest}:{signature}:{fingerprint}\n{tampered}"
        result = self.run_tool(forged)
        self.assertEqual(result.returncode, 6, result.stderr)
        self.assertIn("does not verify", result.stderr)

    def test_body_hash_mismatch_refused(self):
        result = self.run_tool(envelope(self.body, self.key) + "# tail\n")
        self.assertEqual(result.returncode, 4, result.stderr)

    def test_wrong_key_with_trusted_fingerprint_refused(self):
        attacker = Ed25519PrivateKey.generate()
        fingerprint = hashlib.sha256(raw_public(self.key)).hexdigest()
        result = self.run_tool(envelope(self.body, attacker, claimed_fingerprint=fingerprint))
        self.assertEqual(result.returncode, 6, result.stderr)

    def test_unknown_fingerprint_refused(self):
        result = self.run_tool(envelope(self.body, Ed25519PrivateKey.generate()))
        self.assertEqual(result.returncode, 5, result.stderr)
        self.assertIn("not the pinned publisher", result.stderr)

    def test_trust_key_not_matching_fingerprint_refused(self):
        other = Ed25519PrivateKey.generate()
        text = trust_text(other).replace(
            f'fingerprint = "{hashlib.sha256(raw_public(other)).hexdigest()}"',
            f'fingerprint = "{hashlib.sha256(raw_public(self.key)).hexdigest()}"')
        mismatched = self.dir / "mismatched.toml"
        mismatched.write_text(text)
        result = self.run_tool(envelope(self.body, self.key), trust=mismatched)
        self.assertEqual(result.returncode, 5, result.stderr)

    def test_malformed_lines_refused(self):
        good = envelope(self.body, self.key)
        cases = [
            self.body,
            "# ryeos:signed:garbage\n" + self.body,
            good.replace(":", ";", 5),
            good.partition("\n")[0].rsplit(":", 1)[0] + ":abc\n" + self.body,
            good.partition("\n")[0] + "\n" + good.partition("\n")[0] + "\n" + self.body,
        ]
        for text in cases:
            with self.subTest(first=text.split("\n")[0][:40]):
                self.assertEqual(self.run_tool(text).returncode, 3)

    def test_real_signed_file_accepted(self):
        result = subprocess.run([sys.executable, "-I", str(TOOL), "--repository-root", str(ROOT),
                                 "--set", "core", "cargo-features", "ryeos-cli"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(TRUST.is_file())

    def test_real_file_modified_with_recomputed_hash_refused(self):
        first, _, body = CONFIG.read_text().partition("\n")
        body += "# modified\n"
        _, signature, fingerprint = first.rsplit(":", 2)
        digest = hashlib.sha256(body.encode()).hexdigest()
        result = self.run_tool(f"# ryeos:signed:2026-01-01T00:00:00Z:{digest}:{signature}:{fingerprint}\n{body}", trust=TRUST)
        self.assertEqual(result.returncode, 6, result.stderr)


if __name__ == "__main__":
    unittest.main()
