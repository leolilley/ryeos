"""Tests for node key management — generate node identity and authorized-key CRUD."""

from pathlib import Path
from unittest.mock import patch

import pytest


@pytest.fixture
def node_home(tmp_path):
    """Provide a temp user space for node operations."""
    with patch("rye.utils.path_utils.get_user_space", return_value=tmp_path):
        yield tmp_path


def _exec(params, node_home):
    """Import and call node_keys.execute with patched user space."""
    import importlib
    import sys

    tool_path = (
        Path(__file__).resolve().parents[3]
        / "ryeos"
        / "bundles"
        / "core"
        / "ryeos_core"
        / ".ai"
        / "tools"
        / "rye"
        / "core"
        / "keys"
        / "node_keys.py"
    )
    spec = importlib.util.spec_from_file_location("node_keys", tool_path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["node_keys"] = mod
    spec.loader.exec_module(mod)

    with patch("rye.utils.path_utils.get_user_space", return_value=node_home):
        return mod.execute(params, str(node_home / "project"))


class TestNodeKeyGenerate:
    """Test node identity keypair generation."""

    def test_generate_creates_keypair(self, node_home):
        result = _exec({"action": "generate"}, node_home)
        assert result["success"] is True
        assert result["created"] is True
        assert "fingerprint" in result
        identity_dir = node_home / ".ai" / "node" / "identity"
        assert (identity_dir / "private_key.pem").exists()
        assert (identity_dir / "public_key.pem").exists()

    def test_generate_idempotent(self, node_home):
        r1 = _exec({"action": "generate"}, node_home)
        r2 = _exec({"action": "generate"}, node_home)
        assert r1["fingerprint"] == r2["fingerprint"]
        assert r2["created"] is False

    def test_generate_force(self, node_home):
        r1 = _exec({"action": "generate"}, node_home)
        r2 = _exec({"action": "generate", "force": True}, node_home)
        assert r2["created"] is True
        assert r2["fingerprint"] != r1["fingerprint"]

    def test_generate_writes_to_node_identity_not_config(self, node_home):
        _exec({"action": "generate"}, node_home)
        # Must be at ~/.ai/node/identity/, NOT ~/.ai/config/keys/signing/
        assert (node_home / ".ai" / "node" / "identity" / "private_key.pem").exists()
        assert not (node_home / ".ai" / "config" / "keys" / "signing" / "private_key.pem").exists()


class TestNodeKeyInfo:
    """Test node info action."""

    def test_info_no_key(self, node_home):
        result = _exec({"action": "info"}, node_home)
        assert result["success"] is False

    def test_info_after_generate(self, node_home):
        gen = _exec({"action": "generate"}, node_home)
        info = _exec({"action": "info"}, node_home)
        assert info["success"] is True
        assert info["fingerprint"] == gen["fingerprint"]


class TestAuthorizedKeys:
    """Test authorized-key CRUD at ~/.ai/node/authorized-keys/."""

    def _gen_user_key(self):
        """Generate a throwaway user keypair for testing."""
        from rye.primitives.signing import generate_keypair, compute_key_fingerprint
        _, pub = generate_keypair()
        return pub, compute_key_fingerprint(pub)

    def test_authorize_requires_node_key(self, node_home):
        pub, _ = self._gen_user_key()
        result = _exec(
            {"action": "authorize", "public_key_pem": pub.decode()},
            node_home,
        )
        assert result["success"] is False
        assert "generate" in result["error"]

    def test_authorize_and_list(self, node_home):
        _exec({"action": "generate"}, node_home)
        pub, fp = self._gen_user_key()

        result = _exec(
            {"action": "authorize", "public_key_pem": pub.decode(), "label": "test-user"},
            node_home,
        )
        assert result["success"] is True
        assert result["fingerprint"] == fp
        assert result["replaced"] is False

        # File exists at correct path
        key_file = node_home / ".ai" / "node" / "authorized-keys" / f"{fp}.toml"
        assert key_file.exists()
        content = key_file.read_text()
        assert content.startswith("# rye:signed:")
        assert f'fingerprint = "{fp}"' in content

        # List shows it
        ls = _exec({"action": "list"}, node_home)
        assert ls["count"] == 1
        assert ls["keys"][0]["fingerprint"] == fp
        assert ls["keys"][0]["label"] == "test-user"

    def test_authorize_replace(self, node_home):
        _exec({"action": "generate"}, node_home)
        pub, fp = self._gen_user_key()

        _exec({"action": "authorize", "public_key_pem": pub.decode(), "label": "v1"}, node_home)
        r2 = _exec({"action": "authorize", "public_key_pem": pub.decode(), "label": "v2"}, node_home)
        assert r2["replaced"] is True

        ls = _exec({"action": "list"}, node_home)
        assert ls["count"] == 1
        assert ls["keys"][0]["label"] == "v2"

    def test_revoke(self, node_home):
        _exec({"action": "generate"}, node_home)
        pub, fp = self._gen_user_key()

        _exec({"action": "authorize", "public_key_pem": pub.decode()}, node_home)
        assert _exec({"action": "list"}, node_home)["count"] == 1

        result = _exec({"action": "revoke", "fingerprint": fp}, node_home)
        assert result["success"] is True
        assert _exec({"action": "list"}, node_home)["count"] == 0

    def test_revoke_nonexistent(self, node_home):
        _exec({"action": "generate"}, node_home)
        result = _exec({"action": "revoke", "fingerprint": "0000000000000000"}, node_home)
        assert result["success"] is False

    def test_list_empty(self, node_home):
        ls = _exec({"action": "list"}, node_home)
        assert ls["success"] is True
        assert ls["count"] == 0

    def test_authorize_custom_scopes(self, node_home):
        _exec({"action": "generate"}, node_home)
        pub, fp = self._gen_user_key()

        _exec(
            {"action": "authorize", "public_key_pem": pub.decode(), "scopes": ["remote:execute"]},
            node_home,
        )
        ls = _exec({"action": "list"}, node_home)
        assert ls["keys"][0]["scopes"] == ["remote:execute"]

    def test_multiple_authorized_keys(self, node_home):
        _exec({"action": "generate"}, node_home)

        pub1, fp1 = self._gen_user_key()
        pub2, fp2 = self._gen_user_key()

        _exec({"action": "authorize", "public_key_pem": pub1.decode(), "label": "user-a"}, node_home)
        _exec({"action": "authorize", "public_key_pem": pub2.decode(), "label": "user-b"}, node_home)

        ls = _exec({"action": "list"}, node_home)
        assert ls["count"] == 2
        fps = {k["fingerprint"] for k in ls["keys"]}
        assert fps == {fp1, fp2}

    def test_authorize_rejects_toml_injection_label(self, node_home):
        _exec({"action": "generate"}, node_home)
        pub, _ = self._gen_user_key()
        result = _exec(
            {"action": "authorize", "public_key_pem": pub.decode(), "label": 'bad"label'},
            node_home,
        )
        assert result["success"] is False
        assert "label" in result["error"].lower() or "Invalid" in result["error"]

    def test_authorize_rejects_newline_label(self, node_home):
        _exec({"action": "generate"}, node_home)
        pub, _ = self._gen_user_key()
        result = _exec(
            {"action": "authorize", "public_key_pem": pub.decode(), "label": "bad\nlabel"},
            node_home,
        )
        assert result["success"] is False

    def test_authorize_rejects_toml_injection_scope(self, node_home):
        _exec({"action": "generate"}, node_home)
        pub, _ = self._gen_user_key()
        result = _exec(
            {"action": "authorize", "public_key_pem": pub.decode(), "scopes": ['valid', 'bad"scope']},
            node_home,
        )
        assert result["success"] is False

    def test_revoke_rejects_path_traversal(self, node_home):
        _exec({"action": "generate"}, node_home)
        result = _exec({"action": "revoke", "fingerprint": "../../etc/passwd"}, node_home)
        assert result["success"] is False
        assert "fingerprint" in result["error"].lower() or "Invalid" in result["error"]

    def test_revoke_rejects_invalid_fingerprint_format(self, node_home):
        _exec({"action": "generate"}, node_home)
        result = _exec({"action": "revoke", "fingerprint": "ABCD1234"}, node_home)
        assert result["success"] is False
