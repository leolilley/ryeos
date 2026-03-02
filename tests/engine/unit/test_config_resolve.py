"""Tests for the config_resolve mechanism in PrimitiveExecutor and ConfigLoader paths.

Covers:
- _resolve_tool_config / _resolve_single_config: deep_merge and first_match modes
- _deep_merge_config: edge cases (empty dicts, nested merge, list override, extends)
- config_resolve metadata extraction (Python CONFIG_RESOLVE vs YAML config_resolve)
- ConfigLoader system/user/project path layout (.ai/config/agent/)
- Multiple system bundle merging
- Missing config files → empty dict
"""

import sys
import tempfile
from pathlib import Path

import pytest
import yaml

PROJECT_ROOT = Path(__file__).parent.parent.parent

# Ensure rye is importable
sys.path.insert(0, str(PROJECT_ROOT / "ryeos"))

from rye.constants import AI_DIR
from rye.executor.primitive_executor import PrimitiveExecutor, ChainElement
from rye.utils.path_utils import BundleInfo


# ── Helpers ───────────────────────────────────────────────────────────

def _write_yaml(path: Path, data: dict) -> None:
    """Write a YAML file, creating parent directories."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w") as f:
        yaml.dump(data, f)


def _make_executor(
    tmp_path: Path,
    *,
    system_bundles: list[Path] | None = None,
    user_space: Path | None = None,
) -> PrimitiveExecutor:
    """Build a PrimitiveExecutor with controllable space paths."""
    project = tmp_path / "project"
    project.mkdir(exist_ok=True)

    if user_space is None:
        user_space = tmp_path / "user"
        user_space.mkdir(exist_ok=True)

    if system_bundles is None:
        sys_root = tmp_path / "system"
        sys_root.mkdir(exist_ok=True)
        system_bundles = [sys_root]

    executor = PrimitiveExecutor(
        project_path=project,
        user_space=user_space,
        system_space=system_bundles[0],
    )
    # Inject multiple bundles if provided
    if len(system_bundles) > 1:
        executor.system_spaces = [
            BundleInfo(
                bundle_id=f"bundle-{i}",
                version="1.0.0",
                root_path=p,
                manifest_path=None,
                source="test",
            )
            for i, p in enumerate(system_bundles)
        ]

    return executor


# ── _deep_merge_config ────────────────────────────────────────────────

class TestDeepMergeConfig:
    """Edge cases for the _deep_merge_config static method."""

    def test_empty_base_and_override(self):
        assert PrimitiveExecutor._deep_merge_config({}, {}) == {}

    def test_empty_base(self):
        result = PrimitiveExecutor._deep_merge_config({}, {"a": 1})
        assert result == {"a": 1}

    def test_empty_override(self):
        result = PrimitiveExecutor._deep_merge_config({"a": 1}, {})
        assert result == {"a": 1}

    def test_scalar_override(self):
        result = PrimitiveExecutor._deep_merge_config({"x": 1}, {"x": 2})
        assert result == {"x": 2}

    def test_nested_dict_merge(self):
        base = {"db": {"host": "localhost", "port": 5432}}
        override = {"db": {"port": 3306, "name": "prod"}}
        result = PrimitiveExecutor._deep_merge_config(base, override)
        assert result == {"db": {"host": "localhost", "port": 3306, "name": "prod"}}

    def test_deeply_nested_merge(self):
        base = {"a": {"b": {"c": 1, "d": 2}}}
        override = {"a": {"b": {"c": 99}}}
        result = PrimitiveExecutor._deep_merge_config(base, override)
        assert result["a"]["b"]["c"] == 99
        assert result["a"]["b"]["d"] == 2

    def test_extends_key_skipped(self):
        base = {"name": "base"}
        override = {"extends": "parent", "name": "child", "extra": True}
        result = PrimitiveExecutor._deep_merge_config(base, override)
        assert "extends" not in result
        assert result["name"] == "child"
        assert result["extra"] is True

    def test_list_replaces_not_merges(self):
        base = {"items": [1, 2, 3]}
        override = {"items": [4, 5]}
        result = PrimitiveExecutor._deep_merge_config(base, override)
        assert result["items"] == [4, 5]

    def test_dict_replaces_scalar(self):
        base = {"val": "string"}
        override = {"val": {"nested": True}}
        result = PrimitiveExecutor._deep_merge_config(base, override)
        assert result["val"] == {"nested": True}

    def test_scalar_replaces_dict(self):
        base = {"val": {"nested": True}}
        override = {"val": "flat"}
        result = PrimitiveExecutor._deep_merge_config(base, override)
        assert result["val"] == "flat"

    def test_base_not_mutated(self):
        base = {"a": {"b": 1}}
        override = {"a": {"c": 2}}
        PrimitiveExecutor._deep_merge_config(base, override)
        assert base == {"a": {"b": 1}}

    def test_new_keys_added(self):
        base = {"a": 1}
        override = {"b": 2, "c": 3}
        result = PrimitiveExecutor._deep_merge_config(base, override)
        assert result == {"a": 1, "b": 2, "c": 3}


# ── _resolve_single_config ───────────────────────────────────────────

class TestResolveSingleConfig:
    """Test _resolve_single_config with deep_merge and first_match modes."""

    def test_deep_merge_system_only(self, tmp_path):
        executor = _make_executor(tmp_path)
        sys_root = executor.system_spaces[0].root_path

        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "agent.yaml",
            {"provider": "openai", "timeout": 30},
        )

        result = executor._resolve_single_config(
            {"path": "agent/agent.yaml", "mode": "deep_merge"}
        )
        assert result == {"provider": "openai", "timeout": 30}

    def test_deep_merge_system_user_project_cascade(self, tmp_path):
        executor = _make_executor(tmp_path)
        sys_root = executor.system_spaces[0].root_path

        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "agent.yaml",
            {"provider": "openai", "timeout": 30, "retries": 3},
        )
        _write_yaml(
            executor.user_space / AI_DIR / "config" / "agent" / "agent.yaml",
            {"timeout": 60},
        )
        _write_yaml(
            executor.project_path / AI_DIR / "config" / "agent" / "agent.yaml",
            {"provider": "anthropic", "model": "claude"},
        )

        result = executor._resolve_single_config(
            {"path": "agent/agent.yaml", "mode": "deep_merge"}
        )
        assert result["provider"] == "anthropic"  # project overrides system
        assert result["timeout"] == 60  # user overrides system
        assert result["retries"] == 3  # system default preserved
        assert result["model"] == "claude"  # project-only key

    def test_deep_merge_nested_dicts(self, tmp_path):
        executor = _make_executor(tmp_path)
        sys_root = executor.system_spaces[0].root_path

        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "agent.yaml",
            {"db": {"host": "localhost", "port": 5432}},
        )
        _write_yaml(
            executor.project_path / AI_DIR / "config" / "agent" / "agent.yaml",
            {"db": {"port": 3306, "name": "mydb"}},
        )

        result = executor._resolve_single_config(
            {"path": "agent/agent.yaml", "mode": "deep_merge"}
        )
        assert result["db"]["host"] == "localhost"
        assert result["db"]["port"] == 3306
        assert result["db"]["name"] == "mydb"

    def test_deep_merge_skips_extends(self, tmp_path):
        executor = _make_executor(tmp_path)
        sys_root = executor.system_spaces[0].root_path

        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "agent.yaml",
            {"extends": "base", "name": "system"},
        )
        _write_yaml(
            executor.user_space / AI_DIR / "config" / "agent" / "agent.yaml",
            {"extends": "system", "name": "user"},
        )

        result = executor._resolve_single_config(
            {"path": "agent/agent.yaml", "mode": "deep_merge"}
        )
        assert "extends" not in result
        assert result["name"] == "user"

    def test_first_match_project_wins(self, tmp_path):
        executor = _make_executor(tmp_path)
        sys_root = executor.system_spaces[0].root_path

        _write_yaml(
            sys_root / AI_DIR / "config" / "web" / "search.yaml",
            {"engine": "google"},
        )
        _write_yaml(
            executor.project_path / AI_DIR / "config" / "web" / "search.yaml",
            {"engine": "bing"},
        )

        result = executor._resolve_single_config(
            {"path": "web/search.yaml", "mode": "first_match"}
        )
        assert result == {"engine": "bing"}

    def test_first_match_user_wins_when_no_project(self, tmp_path):
        executor = _make_executor(tmp_path)

        _write_yaml(
            executor.user_space / AI_DIR / "config" / "web" / "search.yaml",
            {"engine": "duckduckgo"},
        )

        result = executor._resolve_single_config(
            {"path": "web/search.yaml", "mode": "first_match"}
        )
        assert result == {"engine": "duckduckgo"}

    def test_first_match_system_fallback(self, tmp_path):
        executor = _make_executor(tmp_path)
        sys_root = executor.system_spaces[0].root_path

        _write_yaml(
            sys_root / AI_DIR / "config" / "web" / "search.yaml",
            {"engine": "default"},
        )

        result = executor._resolve_single_config(
            {"path": "web/search.yaml", "mode": "first_match"}
        )
        assert result == {"engine": "default"}

    def test_missing_config_returns_empty_dict(self, tmp_path):
        executor = _make_executor(tmp_path)

        result = executor._resolve_single_config(
            {"path": "nonexistent/config.yaml", "mode": "deep_merge"}
        )
        assert result == {}

    def test_missing_config_first_match_returns_empty_dict(self, tmp_path):
        executor = _make_executor(tmp_path)

        result = executor._resolve_single_config(
            {"path": "nonexistent/config.yaml", "mode": "first_match"}
        )
        assert result == {}

    def test_empty_path_returns_empty_dict(self, tmp_path):
        executor = _make_executor(tmp_path)

        result = executor._resolve_single_config({"path": "", "mode": "deep_merge"})
        assert result == {}

    def test_no_path_key_returns_empty_dict(self, tmp_path):
        executor = _make_executor(tmp_path)

        result = executor._resolve_single_config({"mode": "deep_merge"})
        assert result == {}

    def test_default_mode_is_deep_merge(self, tmp_path):
        executor = _make_executor(tmp_path)
        sys_root = executor.system_spaces[0].root_path

        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "test.yaml",
            {"key": "value"},
        )

        result = executor._resolve_single_config({"path": "agent/test.yaml"})
        assert result == {"key": "value"}


# ── _resolve_tool_config ─────────────────────────────────────────────

class TestResolveToolConfig:
    """Test _resolve_tool_config with single spec and list of specs."""

    def test_single_spec_dict(self, tmp_path):
        executor = _make_executor(tmp_path)
        sys_root = executor.system_spaces[0].root_path

        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "agent.yaml",
            {"provider": "openai"},
        )

        result = executor._resolve_tool_config(
            {"path": "agent/agent.yaml", "mode": "deep_merge"}
        )
        assert result == {"provider": "openai"}

    def test_multiple_specs_list(self, tmp_path):
        executor = _make_executor(tmp_path)
        sys_root = executor.system_spaces[0].root_path

        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "agent.yaml",
            {"provider": "openai"},
        )
        _write_yaml(
            sys_root / AI_DIR / "config" / "web" / "search.yaml",
            {"engine": "google"},
        )

        result = executor._resolve_tool_config([
            {"path": "agent/agent.yaml", "mode": "deep_merge"},
            {"path": "web/search.yaml", "mode": "first_match"},
        ])
        assert result["agent/agent.yaml"] == {"provider": "openai"}
        assert result["web/search.yaml"] == {"engine": "google"}

    def test_invalid_type_returns_empty(self, tmp_path):
        executor = _make_executor(tmp_path)
        assert executor._resolve_tool_config("invalid") == {}
        assert executor._resolve_tool_config(42) == {}
        assert executor._resolve_tool_config(None) == {}


# ── Multiple System Bundles ──────────────────────────────────────────

class TestMultipleSystemBundles:
    """Test config resolution with multiple system bundles."""

    def test_bundles_merged_in_order(self, tmp_path):
        bundle_a = tmp_path / "bundle_a"
        bundle_b = tmp_path / "bundle_b"
        bundle_a.mkdir()
        bundle_b.mkdir()

        _write_yaml(
            bundle_a / AI_DIR / "config" / "agent" / "agent.yaml",
            {"provider": "openai", "model": "gpt-4", "timeout": 30},
        )
        _write_yaml(
            bundle_b / AI_DIR / "config" / "agent" / "agent.yaml",
            {"model": "gpt-4o", "temperature": 0.7},
        )

        executor = _make_executor(
            tmp_path, system_bundles=[bundle_a, bundle_b]
        )

        result = executor._resolve_single_config(
            {"path": "agent/agent.yaml", "mode": "deep_merge"}
        )
        assert result["provider"] == "openai"  # from bundle_a
        assert result["model"] == "gpt-4o"  # bundle_b overrides bundle_a
        assert result["timeout"] == 30  # from bundle_a only
        assert result["temperature"] == 0.7  # from bundle_b only

    def test_first_match_picks_first_bundle(self, tmp_path):
        bundle_a = tmp_path / "bundle_a"
        bundle_b = tmp_path / "bundle_b"
        bundle_a.mkdir()
        bundle_b.mkdir()

        _write_yaml(
            bundle_a / AI_DIR / "config" / "web" / "search.yaml",
            {"engine": "from_a"},
        )
        _write_yaml(
            bundle_b / AI_DIR / "config" / "web" / "search.yaml",
            {"engine": "from_b"},
        )

        executor = _make_executor(
            tmp_path, system_bundles=[bundle_a, bundle_b]
        )

        result = executor._resolve_single_config(
            {"path": "web/search.yaml", "mode": "first_match"}
        )
        # first_match checks project → user → system bundles in order
        assert result["engine"] == "from_a"

    def test_bundle_with_missing_config_skipped(self, tmp_path):
        bundle_a = tmp_path / "bundle_a"
        bundle_b = tmp_path / "bundle_b"
        bundle_a.mkdir()
        bundle_b.mkdir()

        # Only bundle_b has the config
        _write_yaml(
            bundle_b / AI_DIR / "config" / "agent" / "agent.yaml",
            {"provider": "anthropic"},
        )

        executor = _make_executor(
            tmp_path, system_bundles=[bundle_a, bundle_b]
        )

        result = executor._resolve_single_config(
            {"path": "agent/agent.yaml", "mode": "deep_merge"}
        )
        assert result == {"provider": "anthropic"}


# ── Metadata Extraction ──────────────────────────────────────────────

class TestConfigResolveMetadataExtraction:
    """Test that CONFIG_RESOLVE / config_resolve is extracted from parsed metadata."""

    def test_python_style_uppercase(self, tmp_path):
        executor = _make_executor(tmp_path)

        parsed = {
            "__version__": "1.0.0",
            "__tool_type__": "python",
            "CONFIG_RESOLVE": {"path": "agent/agent.yaml", "mode": "deep_merge"},
        }
        metadata = executor._extract_metadata_from_parsed(parsed)
        assert metadata["config_resolve"] == {
            "path": "agent/agent.yaml",
            "mode": "deep_merge",
        }

    def test_yaml_style_lowercase(self, tmp_path):
        executor = _make_executor(tmp_path)

        parsed = {
            "version": "1.0.0",
            "tool_type": "yaml",
            "config_resolve": [
                {"path": "agent/agent.yaml", "mode": "deep_merge"},
                {"path": "web/search.yaml", "mode": "first_match"},
            ],
        }
        metadata = executor._extract_metadata_from_parsed(parsed)
        assert isinstance(metadata["config_resolve"], list)
        assert len(metadata["config_resolve"]) == 2
        assert metadata["config_resolve"][0]["path"] == "agent/agent.yaml"

    def test_yaml_data_sub_dict(self, tmp_path):
        """YAML parser puts fields under a 'data' key."""
        executor = _make_executor(tmp_path)

        parsed = {
            "data": {
                "version": "1.0.0",
                "tool_type": "yaml",
                "config_resolve": {"path": "agent/events.yaml", "mode": "deep_merge"},
            }
        }
        metadata = executor._extract_metadata_from_parsed(parsed)
        assert metadata["config_resolve"]["path"] == "agent/events.yaml"

    def test_no_config_resolve_key(self, tmp_path):
        executor = _make_executor(tmp_path)

        parsed = {
            "__version__": "1.0.0",
            "__tool_type__": "python",
        }
        metadata = executor._extract_metadata_from_parsed(parsed)
        assert "config_resolve" not in metadata


# ── ConfigLoader Path Layout ─────────────────────────────────────────

class TestConfigLoaderPaths:
    """Verify ConfigLoader reads from .ai/config/agent/ at each tier."""

    @pytest.fixture
    def config_loader_mod(self):
        """Import ConfigLoader module and return it for monkeypatching."""
        import importlib.util

        loader_path = (
            PROJECT_ROOT
            / "ryeos" / "bundles" / "standard" / "ryeos_std"
            / ".ai" / "tools" / "rye" / "agent" / "threads" / "loaders"
            / "config_loader.py"
        )
        spec = importlib.util.spec_from_file_location(
            "config_loader_test", loader_path
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod

    def test_system_tier_reads_config_agent(
        self, tmp_path, monkeypatch, config_loader_mod
    ):
        sys_root = tmp_path / "system"
        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "agent.yaml",
            {"provider": "default_provider", "retries": 3},
        )

        bundle = BundleInfo(
            bundle_id="test-core",
            version="1.0.0",
            root_path=sys_root,
            manifest_path=None,
            source="test",
        )
        monkeypatch.setattr(
            config_loader_mod, "get_system_spaces", lambda: [bundle]
        )
        monkeypatch.setattr(
            config_loader_mod, "get_user_ai_path",
            lambda: tmp_path / "user_ai",
        )

        loader = config_loader_mod.ConfigLoader("agent.yaml")
        result = loader.load(tmp_path / "project")

        assert result["provider"] == "default_provider"
        assert result["retries"] == 3

    def test_user_tier_overrides_system(
        self, tmp_path, monkeypatch, config_loader_mod
    ):
        sys_root = tmp_path / "system"
        user_ai = tmp_path / "user_ai"

        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "coordination.yaml",
            {"max_threads": 4, "strategy": "round_robin"},
        )
        _write_yaml(
            user_ai / "config" / "agent" / "coordination.yaml",
            {"max_threads": 8},
        )

        bundle = BundleInfo(
            bundle_id="test-core",
            version="1.0.0",
            root_path=sys_root,
            manifest_path=None,
            source="test",
        )
        monkeypatch.setattr(
            config_loader_mod, "get_system_spaces", lambda: [bundle]
        )
        monkeypatch.setattr(
            config_loader_mod, "get_user_ai_path", lambda: user_ai
        )

        loader = config_loader_mod.ConfigLoader("coordination.yaml")
        result = loader.load(tmp_path / "project")

        assert result["max_threads"] == 8
        assert result["strategy"] == "round_robin"

    def test_project_tier_overrides_all(
        self, tmp_path, monkeypatch, config_loader_mod
    ):
        sys_root = tmp_path / "system"
        user_ai = tmp_path / "user_ai"
        project = tmp_path / "project"

        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "resilience.yaml",
            {"retries": 3, "backoff": "exponential", "timeout": 30},
        )
        _write_yaml(
            user_ai / "config" / "agent" / "resilience.yaml",
            {"retries": 5},
        )
        _write_yaml(
            project / AI_DIR / "config" / "agent" / "resilience.yaml",
            {"timeout": 120, "circuit_breaker": True},
        )

        bundle = BundleInfo(
            bundle_id="test-core",
            version="1.0.0",
            root_path=sys_root,
            manifest_path=None,
            source="test",
        )
        monkeypatch.setattr(
            config_loader_mod, "get_system_spaces", lambda: [bundle]
        )
        monkeypatch.setattr(
            config_loader_mod, "get_user_ai_path", lambda: user_ai
        )

        loader = config_loader_mod.ConfigLoader("resilience.yaml")
        result = loader.load(project)

        assert result["retries"] == 5  # user override
        assert result["backoff"] == "exponential"  # system default
        assert result["timeout"] == 120  # project override
        assert result["circuit_breaker"] is True  # project-only

    def test_no_config_files_returns_empty(
        self, tmp_path, monkeypatch, config_loader_mod
    ):
        monkeypatch.setattr(
            config_loader_mod, "get_system_spaces", lambda: []
        )
        monkeypatch.setattr(
            config_loader_mod, "get_user_ai_path",
            lambda: tmp_path / "nonexistent",
        )

        loader = config_loader_mod.ConfigLoader("missing.yaml")
        result = loader.load(tmp_path / "no_project")
        assert result == {}

    def test_cache_returns_same_result(
        self, tmp_path, monkeypatch, config_loader_mod
    ):
        sys_root = tmp_path / "system"
        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "agent.yaml",
            {"cached": True},
        )

        bundle = BundleInfo(
            bundle_id="test-core",
            version="1.0.0",
            root_path=sys_root,
            manifest_path=None,
            source="test",
        )
        monkeypatch.setattr(
            config_loader_mod, "get_system_spaces", lambda: [bundle]
        )
        monkeypatch.setattr(
            config_loader_mod, "get_user_ai_path",
            lambda: tmp_path / "user_ai",
        )

        loader = config_loader_mod.ConfigLoader("agent.yaml")
        project = tmp_path / "project"
        r1 = loader.load(project)
        r2 = loader.load(project)
        assert r1 is r2  # same object from cache

    def test_clear_cache(self, tmp_path, monkeypatch, config_loader_mod):
        sys_root = tmp_path / "system"
        _write_yaml(
            sys_root / AI_DIR / "config" / "agent" / "agent.yaml",
            {"version": 1},
        )

        bundle = BundleInfo(
            bundle_id="test-core",
            version="1.0.0",
            root_path=sys_root,
            manifest_path=None,
            source="test",
        )
        monkeypatch.setattr(
            config_loader_mod, "get_system_spaces", lambda: [bundle]
        )
        monkeypatch.setattr(
            config_loader_mod, "get_user_ai_path",
            lambda: tmp_path / "user_ai",
        )

        loader = config_loader_mod.ConfigLoader("agent.yaml")
        project = tmp_path / "project"
        r1 = loader.load(project)
        loader.clear_cache()
        r2 = loader.load(project)
        assert r1 is not r2
        assert r1 == r2

    def test_multiple_system_bundles_merged(
        self, tmp_path, monkeypatch, config_loader_mod
    ):
        bundle_a = tmp_path / "bundle_a"
        bundle_b = tmp_path / "bundle_b"

        _write_yaml(
            bundle_a / AI_DIR / "config" / "agent" / "events.yaml",
            {"events": {"build": True}},
        )
        _write_yaml(
            bundle_b / AI_DIR / "config" / "agent" / "events.yaml",
            {"events": {"deploy": True}},
        )

        bundles = [
            BundleInfo("a", "1.0.0", bundle_a, None, "test"),
            BundleInfo("b", "1.0.0", bundle_b, None, "test"),
        ]
        monkeypatch.setattr(
            config_loader_mod, "get_system_spaces", lambda: bundles
        )
        monkeypatch.setattr(
            config_loader_mod, "get_user_ai_path",
            lambda: tmp_path / "user_ai",
        )

        loader = config_loader_mod.ConfigLoader("events.yaml")
        result = loader.load(tmp_path / "project")

        assert result["events"]["build"] is True
        assert result["events"]["deploy"] is True


# ── ChainElement config_resolve field ────────────────────────────────

class TestChainElementConfigResolve:
    """Verify config_resolve is wired through ChainElement dataclass."""

    def test_config_resolve_stored_on_element(self):
        element = ChainElement(
            item_id="my/tool",
            path=Path("/tmp/tool.py"),
            space="project",
            config_resolve={"path": "agent/agent.yaml", "mode": "deep_merge"},
        )
        assert element.config_resolve is not None
        assert element.config_resolve["path"] == "agent/agent.yaml"

    def test_config_resolve_defaults_to_none(self):
        element = ChainElement(
            item_id="my/tool",
            path=Path("/tmp/tool.py"),
            space="project",
        )
        assert element.config_resolve is None
