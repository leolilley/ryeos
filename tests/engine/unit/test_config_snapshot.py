"""Tests for config snapshot hashing (Step 5)."""

import tempfile
from pathlib import Path

import yaml

from rye.cas.config_snapshot import compute_config_hash, compute_agent_config_snapshot


class TestComputeConfigHash:
    """Test deterministic config hashing."""

    def test_same_config_same_hash(self):
        config = {"agent.yaml": {"provider": {"default": "anthropic"}}}
        h1 = compute_config_hash(config)
        h2 = compute_config_hash(config)
        assert h1 == h2
        assert len(h1) == 64  # SHA256 hex

    def test_different_config_different_hash(self):
        c1 = {"agent.yaml": {"provider": {"default": "anthropic"}}}
        c2 = {"agent.yaml": {"provider": {"default": "openai"}}}
        assert compute_config_hash(c1) != compute_config_hash(c2)

    def test_key_order_irrelevant(self):
        """Canonical JSON sorts keys — order shouldn't matter."""
        c1 = {"a": 1, "b": 2}
        c2 = {"b": 2, "a": 1}
        assert compute_config_hash(c1) == compute_config_hash(c2)

    def test_empty_config(self):
        h = compute_config_hash({})
        assert len(h) == 64

    def test_nested_config(self):
        config = {
            "agent.yaml": {"provider": {"default": None}, "max_output_tokens": 16384},
            "resilience.yaml": {"retry": {"max_retries": 3}},
        }
        h = compute_config_hash(config)
        assert len(h) == 64


class TestComputeAgentConfigSnapshot:
    """Test aggregated agent config snapshot."""

    def test_snapshot_with_real_system_configs(self, _setup_user_space):
        """Should produce a hash from system defaults."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai").mkdir()

            snapshot_hash, resolved = compute_agent_config_snapshot(project)
            assert len(snapshot_hash) == 64
            assert isinstance(resolved, dict)

    def test_project_override_changes_hash(self, _setup_user_space):
        """Adding a project config should change the snapshot hash."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai").mkdir()

            h1, _ = compute_agent_config_snapshot(project)

            # Add project override
            config_dir = project / ".ai" / "config" / "agent"
            config_dir.mkdir(parents=True)
            (config_dir / "agent.yaml").write_text(
                yaml.dump({"max_output_tokens": 32768})
            )

            h2, resolved = compute_agent_config_snapshot(project)
            assert h1 != h2
            # The override should be reflected
            agent_config = resolved.get("agent.yaml", {})
            assert agent_config.get("max_output_tokens") == 32768

    def test_same_config_same_hash_deterministic(self, _setup_user_space):
        """Same project state → same hash."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai").mkdir()

            h1, _ = compute_agent_config_snapshot(project)
            h2, _ = compute_agent_config_snapshot(project)
            assert h1 == h2
