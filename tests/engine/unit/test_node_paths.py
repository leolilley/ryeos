"""Tests for node path getters — user-space only, data-driven from NodeDir constants."""

from pathlib import Path
from unittest.mock import patch

import pytest

from rye.constants import NodeDir
from rye.utils.path_utils import get_node_dir, get_node_path


class TestGetNodeDir:
    """Test node root directory resolution."""

    def test_returns_user_space_node(self, tmp_path):
        """get_node_dir() returns {USER_SPACE}/.ai/node/."""
        with patch("rye.utils.path_utils.get_user_space", return_value=tmp_path):
            result = get_node_dir()
        assert result == tmp_path / ".ai" / "node"

    def test_does_not_create_directory(self, tmp_path):
        """get_node_dir() returns the path but does NOT create it."""
        with patch("rye.utils.path_utils.get_user_space", return_value=tmp_path):
            result = get_node_dir()
        assert not result.exists()

    def test_never_uses_project_space(self, tmp_path):
        """Node dir does not accept or use a project_path argument.
        Unlike get_project_type_path(), node getters take no project arg."""
        import inspect

        sig = inspect.signature(get_node_dir)
        assert "project_path" not in sig.parameters


class TestGetNodePath:
    """Test node subdirectory resolution — data-driven via NodeDir constants."""

    @pytest.mark.parametrize("subdir", NodeDir.ALL)
    def test_valid_subdir_creates_directory(self, tmp_path, subdir):
        """Each valid NodeDir constant creates its subdirectory on first access."""
        with patch("rye.utils.path_utils.get_user_space", return_value=tmp_path):
            result = get_node_path(subdir)
        assert result.exists()
        assert result.is_dir()
        assert result.name == subdir

    @pytest.mark.parametrize("subdir", NodeDir.ALL)
    def test_valid_subdir_returns_correct_path(self, tmp_path, subdir):
        """Each subdirectory is under ~/.ai/node/{subdir}/."""
        with patch("rye.utils.path_utils.get_user_space", return_value=tmp_path):
            result = get_node_path(subdir)
        assert result == tmp_path / ".ai" / "node" / subdir

    def test_invalid_subdir_raises(self, tmp_path):
        """Invalid subdirectory name raises ValueError."""
        with patch("rye.utils.path_utils.get_user_space", return_value=tmp_path):
            with pytest.raises(ValueError, match="Invalid node subdir 'bogus'"):
                get_node_path("bogus")

    def test_empty_subdir_raises(self, tmp_path):
        """Empty string raises ValueError."""
        with patch("rye.utils.path_utils.get_user_space", return_value=tmp_path):
            with pytest.raises(ValueError, match="Invalid node subdir"):
                get_node_path("")

    def test_idempotent_creation(self, tmp_path):
        """Calling get_node_path() twice creates the directory once, no errors."""
        with patch("rye.utils.path_utils.get_user_space", return_value=tmp_path):
            first = get_node_path(NodeDir.IDENTITY)
            second = get_node_path(NodeDir.IDENTITY)
        assert first == second
        assert first.exists()

    def test_ignores_project_space_node_dir(self, tmp_path):
        """A fake project-space .ai/node/ does not affect node path resolution."""
        fake_project = tmp_path / "project"
        fake_node = fake_project / ".ai" / "node" / "identity"
        fake_node.mkdir(parents=True)
        (fake_node / "fake_key.pem").write_text("FAKE")

        with patch(
            "rye.utils.path_utils.get_user_space", return_value=tmp_path / "home"
        ):
            result = get_node_path(NodeDir.IDENTITY)

        # Result should be under user space, not the fake project space
        assert fake_project not in result.parents
        assert result == tmp_path / "home" / ".ai" / "node" / "identity"


class TestNodeDirConstants:
    """Test that NodeDir constants are complete and consistent."""

    def test_dir_constant_is_node(self):
        """NodeDir.DIR is 'node'."""
        assert NodeDir.DIR == "node"

    def test_all_contains_all_subdirs(self):
        """NodeDir.ALL lists every subdirectory constant."""
        expected = {
            NodeDir.IDENTITY,
            NodeDir.ATTESTATION,
            NodeDir.AUTHORIZED_KEYS,
            NodeDir.VAULT,
            NodeDir.EXECUTIONS,
            NodeDir.LOGS,
        }
        assert set(NodeDir.ALL) == expected

    def test_all_has_six_entries(self):
        """Six node subdirectories: identity, attestation, authorized-keys, vault, executions, logs."""
        assert len(NodeDir.ALL) == 6
