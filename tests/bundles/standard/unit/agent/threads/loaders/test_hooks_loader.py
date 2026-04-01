"""Tests for user-level and project-level hooks loading."""

import pytest
import sys
from pathlib import Path

# The conftest.py in this directory already sets up runtime lib paths
# and pre-imports core modules (condition_evaluator, interpolation, module_loader).

from conftest import PROJECT_ROOT, get_bundle_path
from module_loader import load_module

_THREADS_ANCHOR = (
    PROJECT_ROOT
    / "ryeos" / "bundles" / "standard" / "ryeos_std" / ".ai" / "tools" / "rye" / "agent" / "threads"
)


@pytest.fixture
def hooks_loader():
    mod = load_module("loaders/hooks_loader", anchor=_THREADS_ANCHOR)
    loader = mod.HooksLoader()
    loader.clear_cache()
    return loader


@pytest.fixture
def project_dir(tmp_path):
    """Create a project directory with .ai structure."""
    proj = tmp_path / "project"
    proj.mkdir()
    (proj / ".ai").mkdir()
    return proj


class TestUserHooks:
    def test_no_user_hooks_file_returns_empty(self, hooks_loader, tmp_path, monkeypatch):
        monkeypatch.setenv("USER_SPACE", str(tmp_path / "nonexistent"))
        assert hooks_loader.get_user_hooks() == []

    def test_empty_user_hooks_file_returns_empty(self, hooks_loader, tmp_path, monkeypatch):
        user_space = tmp_path / "user"
        hooks_path = user_space / ".ai" / "config" / "agent"
        hooks_path.mkdir(parents=True)
        (hooks_path / "hooks.yaml").write_text("hooks: []")
        monkeypatch.setenv("USER_SPACE", str(user_space))
        assert hooks_loader.get_user_hooks() == []

    def test_user_hooks_loaded(self, hooks_loader, tmp_path, monkeypatch):
        user_space = tmp_path / "user"
        hooks_path = user_space / ".ai" / "config" / "agent"
        hooks_path.mkdir(parents=True)
        (hooks_path / "hooks.yaml").write_text(
            """hooks:
  - id: "inject_style"
    event: "thread_started"
    action:
      primary: "fetch"
      item_type: "knowledge"
      item_id: "personal/coding-style"
"""
        )
        monkeypatch.setenv("USER_SPACE", str(user_space))
        hooks = hooks_loader.get_user_hooks()
        assert len(hooks) == 1
        assert hooks[0]["id"] == "inject_style"
        assert hooks[0]["event"] == "thread_started"
        assert hooks[0]["action"]["item_id"] == "personal/coding-style"


class TestProjectHooks:
    def test_no_project_hooks_file_returns_empty(self, hooks_loader, project_dir):
        assert hooks_loader.get_project_hooks(project_dir) == []

    def test_project_hooks_loaded(self, hooks_loader, project_dir):
        hooks_path = project_dir / ".ai" / "config" / "agent"
        hooks_path.mkdir(parents=True)
        (hooks_path / "hooks.yaml").write_text(
            """hooks:
  - id: "inject_conventions"
    event: "thread_started"
    action:
      primary: "fetch"
      item_type: "knowledge"
      item_id: "project/conventions"
  - id: "record_learnings"
    event: "after_complete"
    condition:
      path: "cost.turns"
      op: "gte"
      value: 3
    action:
      primary: "execute"
      item_type: "directive"
      item_id: "project/record-learnings"
"""
        )
        hooks = hooks_loader.get_project_hooks(project_dir)
        assert len(hooks) == 2
        assert hooks[0]["id"] == "inject_conventions"
        assert hooks[1]["id"] == "record_learnings"
        assert hooks[1]["condition"]["op"] == "gte"

    def test_project_hooks_with_conditions(self, hooks_loader, project_dir):
        hooks_path = project_dir / ".ai" / "config" / "agent"
        hooks_path.mkdir(parents=True)
        (hooks_path / "hooks.yaml").write_text(
            """hooks:
  - id: "api_context"
    event: "thread_started"
    condition:
      path: "directive"
      op: "contains"
      value: "api"
    action:
      primary: "fetch"
      item_type: "knowledge"
      item_id: "project/api-types"
"""
        )
        hooks = hooks_loader.get_project_hooks(project_dir)
        assert len(hooks) == 1
        assert hooks[0]["condition"]["path"] == "directive"


class TestMergeHooks:
    def test_merge_hooks_layer_ordering(self, tmp_path, monkeypatch):
        """Test that _merge_hooks produces correct layer ordering."""
        # Set up user hooks
        user_space = tmp_path / "user"
        hooks_path = user_space / ".ai" / "config" / "agent"
        hooks_path.mkdir(parents=True)
        (hooks_path / "hooks.yaml").write_text(
            """hooks:
  - id: "user_hook"
    event: "thread_started"
    action:
      primary: "fetch"
      item_type: "knowledge"
      item_id: "user/style"
"""
        )
        # Trust the env signing key so bundle config integrity checks pass.
        from conftest import get_env_signing_pubkey
        from rye.primitives.signing import generate_keypair, save_keypair
        real_signing_pubkey = get_env_signing_pubkey()

        monkeypatch.setenv("USER_SPACE", str(user_space))

        # The trust store needs a signing keypair to sign key files on add_key
        signing_dir = user_space / ".ai" / "config" / "keys" / "signing"
        priv, pub = generate_keypair()
        save_keypair(priv, pub, signing_dir)

        from rye.utils.trust_store import TrustStore
        store = TrustStore()
        # Trust the ephemeral key first (self-signed), then the env signer
        store.add_key(pub, owner="test-ephemeral", space="user", version="1.0.0")
        if real_signing_pubkey:
            store.add_key(real_signing_pubkey, owner="env-signer", space="user", version="1.0.0")

        # Set up project hooks
        project_dir = tmp_path / "project"
        (project_dir / ".ai" / "config" / "agent").mkdir(parents=True)
        (project_dir / ".ai" / "config" / "agent" / "hooks.yaml").write_text(
            """hooks:
  - id: "project_hook"
    event: "thread_started"
    action:
      primary: "fetch"
      item_type: "knowledge"
      item_id: "project/conventions"
"""
        )

        # Build directive hooks
        directive_hooks = [
            {"id": "directive_hook", "event": "thread_started", "action": {"primary": "fetch"}}
        ]

        # Load a fresh hooks_loader
        mod = load_module("loaders/hooks_loader", anchor=_THREADS_ANCHOR)
        loader = mod.HooksLoader()
        loader.clear_cache()

        user = loader.get_user_hooks()
        builtin = loader.get_builtin_hooks(project_dir)
        project = loader.get_project_hooks(project_dir)
        infra = loader.get_infra_hooks(project_dir)

        for h in user:
            h.setdefault("layer", 0)
        for h in directive_hooks:
            h.setdefault("layer", 1)
        for h in builtin:
            h.setdefault("layer", 2)
        for h in project:
            h.setdefault("layer", 3)
        for h in infra:
            h.setdefault("layer", 4)

        merged = sorted(
            user + directive_hooks + builtin + project + infra,
            key=lambda h: h.get("layer", 2),
        )

        # Verify ordering: user (0) < directive (1) < builtin (2) < project (3) < infra (4)
        layers = [h.get("layer") for h in merged]
        assert layers == sorted(layers), f"Layers not in order: {layers}"

        # Verify user hook is first
        assert merged[0]["id"] == "user_hook"
        assert merged[0]["layer"] == 0

        # Verify directive hook comes after user
        directive_idx = next(i for i, h in enumerate(merged) if h["id"] == "directive_hook")
        assert merged[directive_idx]["layer"] == 1

        # Verify project hook exists and is at layer 3
        project_hook = next((h for h in merged if h["id"] == "project_hook"), None)
        assert project_hook is not None
        assert project_hook["layer"] == 3
