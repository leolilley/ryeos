"""Tests for ExecutionContext — explicit path isolation with no env fallbacks.

Verifies that:
- ExecutionContext.from_env() reads env vars once at the boundary
- TrustStore uses ctx paths, not os.environ
- verify_item uses ctx paths, not os.environ
- Two concurrent contexts with different user_spaces are isolated
- Bootstrap scenario: signing with explicit signing_key_dir works
"""

import os
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from rye.constants import AI_DIR, ItemType
from rye.primitives.signing import (
    compute_key_fingerprint,
    generate_keypair,
    save_keypair,
)
from rye.utils.execution_context import ExecutionContext
from rye.utils.integrity import IntegrityError, verify_item
from rye.utils.metadata_manager import MetadataManager
from rye.utils.path_utils import get_system_spaces
from rye.utils.trust_store import TrustStore


TRUSTED_KEYS_DIR = "config/keys/trusted"


def _make_context(
    tmp: Path,
    *,
    name: str = "default",
) -> tuple[ExecutionContext, bytes]:
    """Create an ExecutionContext with a fresh keypair in a temp directory.

    Returns (ctx, public_key_pem).
    """
    base = tmp / name
    project = base / "project"
    user_space = base / "user"
    signing_dir = user_space / AI_DIR / "config" / "keys" / "signing"

    project.mkdir(parents=True)
    user_space.mkdir(parents=True)

    priv, pub = generate_keypair()
    save_keypair(priv, pub, signing_dir)

    ctx = ExecutionContext(
        project_path=project,
        user_space=user_space,
        signing_key_dir=signing_dir,
        system_spaces=tuple(get_system_spaces()),
    )
    return ctx, pub


class TestExecutionContextFromEnv:
    """from_env() reads env vars once — the only place they're consulted."""

    def test_from_env_uses_user_space_env(self, monkeypatch, tmp_path):
        user = tmp_path / "user"
        user.mkdir()
        monkeypatch.setenv("USER_SPACE", str(user))
        ctx = ExecutionContext.from_env(project_path=tmp_path)
        assert ctx.user_space == user

    def test_from_env_uses_signing_key_dir_env(self, monkeypatch, tmp_path):
        key_dir = tmp_path / "keys"
        key_dir.mkdir()
        monkeypatch.setenv("RYE_SIGNING_KEY_DIR", str(key_dir))
        ctx = ExecutionContext.from_env(project_path=tmp_path)
        assert ctx.signing_key_dir == key_dir

    def test_from_env_defaults_project_to_cwd(self, monkeypatch, tmp_path):
        monkeypatch.chdir(tmp_path)
        ctx = ExecutionContext.from_env()
        assert ctx.project_path == tmp_path

    def test_frozen(self, tmp_path):
        ctx, _ = _make_context(tmp_path)
        with pytest.raises(AttributeError):
            ctx.project_path = tmp_path / "other"


class TestTrustStoreIsolation:
    """TrustStore uses ctx paths, ignoring os.environ."""

    def test_add_key_writes_to_ctx_user_space(self, tmp_path):
        """Key file lands under ctx.user_space, not Path.home()."""
        ctx, pub = _make_context(tmp_path)
        store = TrustStore(ctx)

        fp = store.add_key(pub, owner="test", version="1.0.0")

        expected_file = ctx.user_space / AI_DIR / TRUSTED_KEYS_DIR / f"{fp}.toml"
        assert expected_file.is_file()

    def test_add_key_not_in_home(self, tmp_path, monkeypatch):
        """Key is NOT written to ~/.ai/ when ctx.user_space differs."""
        ctx, pub = _make_context(tmp_path)
        home = tmp_path / "fake_home"
        home.mkdir()
        monkeypatch.setenv("HOME", str(home))

        store = TrustStore(ctx)
        fp = store.add_key(pub, owner="test", version="1.0.0")

        home_key = home / AI_DIR / TRUSTED_KEYS_DIR / f"{fp}.toml"
        assert not home_key.exists()

    def test_get_key_finds_in_ctx_user_space(self, tmp_path):
        """TrustStore resolves keys from ctx.user_space."""
        ctx, pub = _make_context(tmp_path)
        store = TrustStore(ctx)
        fp = store.add_key(pub, owner="test", version="1.0.0")

        found = store.get_key(fp)
        assert found is not None
        assert found.fingerprint == fp

    def test_get_key_ignores_env_user_space(self, tmp_path, monkeypatch):
        """Changing USER_SPACE env var doesn't affect an already-built TrustStore."""
        ctx, pub = _make_context(tmp_path)
        store = TrustStore(ctx)
        fp = store.add_key(pub, owner="test", version="1.0.0")

        # Point USER_SPACE somewhere else — store should still find the key
        other = tmp_path / "other_user"
        other.mkdir()
        monkeypatch.setenv("USER_SPACE", str(other))

        found = store.get_key(fp)
        assert found is not None

    def test_signing_uses_ctx_key_dir(self, tmp_path):
        """add_key signs the TOML with the key from ctx.signing_key_dir."""
        ctx, pub = _make_context(tmp_path)
        store = TrustStore(ctx)
        fp = store.add_key(pub, owner="test", version="1.0.0")

        key_file = ctx.user_space / AI_DIR / TRUSTED_KEYS_DIR / f"{fp}.toml"
        content = key_file.read_text()

        # Should contain a rye:signed header signed by ctx's key
        assert "# rye:signed:" in content

        # The signing fingerprint should match the ctx keypair
        from rye.primitives.signing import load_keypair

        _, ctx_pub = load_keypair(ctx.signing_key_dir)
        ctx_fp = compute_key_fingerprint(ctx_pub)
        assert ctx_fp in content


class TestConcurrentContextIsolation:
    """Two contexts with different user_spaces see different trusted keys."""

    def test_two_contexts_isolated(self, tmp_path):
        ctx_a, pub_a = _make_context(tmp_path, name="alice")
        ctx_b, pub_b = _make_context(tmp_path, name="bob")

        store_a = TrustStore(ctx_a)
        store_b = TrustStore(ctx_b)

        fp_a = store_a.add_key(pub_a, owner="alice", version="1.0.0")
        fp_b = store_b.add_key(pub_b, owner="bob", version="1.0.0")

        # Each store sees only its own key
        assert store_a.get_key(fp_a) is not None
        assert store_a.get_key(fp_b) is None

        assert store_b.get_key(fp_b) is not None
        assert store_b.get_key(fp_a) is None

    def test_verify_item_uses_correct_context(self, tmp_path):
        """verify_item with ctx_a trusts key_a but not key_b."""
        ctx_a, pub_a = _make_context(tmp_path, name="alice")
        ctx_b, pub_b = _make_context(tmp_path, name="bob")

        # Trust key_a in context_a
        fp_a = compute_key_fingerprint(pub_a)
        store_a = TrustStore(ctx_a)
        store_a.add_key(pub_a, owner="alice", version="1.0.0")

        # Sign a tool using alice's key
        tool_dir = ctx_a.project_path / ".ai" / "tools"
        tool_dir.mkdir(parents=True)
        tool_file = tool_dir / "test.py"
        content = '# test tool\n__version__ = "1.0.0"\n'
        signed = MetadataManager.sign_content(
            ItemType.TOOL, content,
            file_path=tool_file,
            signing_key_dir=ctx_a.signing_key_dir,
        )
        tool_file.write_text(signed)

        # Verify with alice's context — should pass
        result = verify_item(tool_file, ItemType.TOOL, ctx=ctx_a)
        assert result != "unverified"

        # Verify with bob's context — should fail (untrusted key)
        # Copy the signed tool to bob's project space
        bob_tool_dir = ctx_b.project_path / ".ai" / "tools"
        bob_tool_dir.mkdir(parents=True)
        bob_tool_file = bob_tool_dir / "test.py"
        bob_tool_file.write_text(signed)

        with pytest.raises(IntegrityError, match="Untrusted key"):
            verify_item(bob_tool_file, ItemType.TOOL, ctx=ctx_b)


class TestBootstrapScenario:
    """Simulates the node bootstrap: signing_key_dir != default location."""

    def test_bootstrap_trust_with_foreign_signing_dir(self, tmp_path):
        """Bootstrap can add a caller's key using the node's signing dir.

        The node's signing key lives at /cas/signing/, not ~/.ai/.
        The caller's public key is the one being trusted.
        """
        # Node's signing key (at /cas/signing/)
        node_signing_dir = tmp_path / "cas" / "signing"
        node_priv, node_pub = generate_keypair()
        save_keypair(node_priv, node_pub, node_signing_dir)
        node_fp = compute_key_fingerprint(node_pub)

        # Caller's key (the one to trust)
        caller_priv, caller_pub = generate_keypair()
        caller_fp = compute_key_fingerprint(caller_pub)

        # Node's user space (where trusted keys are stored)
        node_user = tmp_path / "root"
        node_user.mkdir()

        # Build context with node's signing dir
        ctx = ExecutionContext(
            project_path=node_user,
            user_space=node_user,
            signing_key_dir=node_signing_dir,
            system_spaces=tuple(get_system_spaces()),
        )

        # Trust the node's own key first (so cross-signed keys verify)
        store = TrustStore(ctx)
        store.add_key(node_pub, owner="node", version="1.0.0")

        # Now trust the caller's key (cross-signed by node)
        store.add_key(caller_pub, owner="caller", version="1.0.0")

        # Verify both keys are found
        assert store.get_key(node_fp) is not None
        assert store.get_key(caller_fp) is not None

        # The caller's key file should be signed by the node's key
        key_file = node_user / AI_DIR / TRUSTED_KEYS_DIR / f"{caller_fp}.toml"
        content = key_file.read_text()
        assert node_fp in content  # cross-signed by node

    def test_bootstrap_fails_without_keypair(self, tmp_path):
        """If signing_key_dir has no keypair, add_key raises — not silent failure."""
        empty_signing = tmp_path / "empty_signing"
        empty_signing.mkdir()

        ctx = ExecutionContext(
            project_path=tmp_path,
            user_space=tmp_path,
            signing_key_dir=empty_signing,
            system_spaces=tuple(get_system_spaces()),
        )

        _, pub = generate_keypair()
        store = TrustStore(ctx)

        with pytest.raises(RuntimeError, match="No signing keypair found"):
            store.add_key(pub, owner="test", version="1.0.0")


class TestNoEnvFallback:
    """Verify there are no hidden env-var fallbacks in the trust chain."""

    def test_trust_store_requires_ctx(self):
        """TrustStore cannot be constructed without an ExecutionContext."""
        with pytest.raises(TypeError):
            TrustStore()

    def test_verify_item_requires_ctx(self, tmp_path):
        """verify_item cannot be called with project_path= (old API)."""
        tool_file = tmp_path / "test.py"
        tool_file.write_text("pass\n")
        with pytest.raises(TypeError):
            verify_item(tool_file, ItemType.TOOL, project_path=tmp_path)
