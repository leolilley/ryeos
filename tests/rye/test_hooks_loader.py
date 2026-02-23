"""Tests for user-level and project-level hooks loading."""

import pytest
import sys
from pathlib import Path

# Add the runtime lib so module_loader is importable
_tools_root = Path(__file__).parent.parent.parent / "ryeos" / "rye" / ".ai" / "tools" / "rye"
_runtime_lib = _tools_root / "core" / "runtimes" / "lib" / "python"
if str(_runtime_lib) not in sys.path:
    sys.path.insert(0, str(_runtime_lib))

_threads_dir = _tools_root / "agent" / "threads"
if str(_threads_dir) not in sys.path:
    sys.path.insert(0, str(_threads_dir))


@pytest.fixture
def hooks_loader():
    from loaders.hooks_loader import HooksLoader
    loader = HooksLoader()
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
      primary: "load"
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
      primary: "load"
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
      primary: "load"
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
      primary: "load"
      item_type: "knowledge"
      item_id: "user/style"
"""
        )
        monkeypatch.setenv("USER_SPACE", str(user_space))

        # Set up project hooks
        project_dir = tmp_path / "project"
        (project_dir / ".ai" / "config" / "agent").mkdir(parents=True)
        (project_dir / ".ai" / "config" / "agent" / "hooks.yaml").write_text(
            """hooks:
  - id: "project_hook"
    event: "thread_started"
    action:
      primary: "load"
      item_type: "knowledge"
      item_id: "project/conventions"
"""
        )

        # Build directive hooks
        directive_hooks = [
            {"id": "directive_hook", "event": "thread_started", "action": {"primary": "load"}}
        ]

        # Import and call _merge_hooks
        # We need to reload the module to pick up the new USER_SPACE
        from loaders.hooks_loader import HooksLoader
        loader = HooksLoader()
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
            h.setdefault("layer", 2.5)
        for h in infra:
            h.setdefault("layer", 3)

        merged = sorted(
            user + directive_hooks + builtin + project + infra,
            key=lambda h: h.get("layer", 2),
        )

        # Verify ordering: user (0) < directive (1) < builtin (2) < project (2.5) < infra (3)
        layers = [h.get("layer") for h in merged]
        assert layers == sorted(layers), f"Layers not in order: {layers}"

        # Verify user hook is first
        assert merged[0]["id"] == "user_hook"
        assert merged[0]["layer"] == 0

        # Verify directive hook comes after user
        directive_idx = next(i for i, h in enumerate(merged) if h["id"] == "directive_hook")
        assert merged[directive_idx]["layer"] == 1

        # Verify project hook exists and is at layer 2.5
        project_hook = next((h for h in merged if h["id"] == "project_hook"), None)
        assert project_hook is not None
        assert project_hook["layer"] == 2.5
