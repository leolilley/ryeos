"""Tests for remote_config — named remote resolution."""

import importlib.util
import sys
from pathlib import Path
from unittest.mock import patch

import pytest

from conftest import get_bundle_path

_REMOTE_PATH = get_bundle_path("core", "tools/rye/core/remote/remote.py")

# remote_config lives in a bundle tool dir
_REMOTE_TOOL_DIR = str(get_bundle_path("core", "tools/rye/core/remote"))
if _REMOTE_TOOL_DIR not in sys.path:
    sys.path.insert(0, _REMOTE_TOOL_DIR)

from remote_config import (
    RemoteConfig,
    resolve_remote,
    get_project_path,
    list_remotes,
)


def _patch_config(config):
    """Patch _load_remote_config to return a fixed dict."""
    return patch("remote_config._load_remote_config", return_value=config)


class TestResolveRemote:
    """Tests for resolve_remote()."""

    def test_named_remote_from_config(self):
        config = {
            "remotes": {
                "gpu": {"url": "https://gpu.example.com", "node_id": "fp:abc123"},
            },
        }
        with _patch_config(config):
            rc = resolve_remote("gpu")
        assert rc == RemoteConfig(name="gpu", url="https://gpu.example.com", node_id="fp:abc123")

    def test_default_remote_from_config(self):
        config = {
            "remotes": {
                "default": {"url": "https://default.example.com", "node_id": "fp:def456"},
            },
        }
        with _patch_config(config):
            rc = resolve_remote()
        assert rc.name == "default"
        assert rc.url == "https://default.example.com"

    def test_no_remotes_configured(self):
        with _patch_config({}):
            with pytest.raises(ValueError, match="not found"):
                resolve_remote()

    def test_named_remote_not_found(self):
        with _patch_config({"remotes": {"default": {"url": "x"}}}):
            with pytest.raises(ValueError, match="'gpu' not found"):
                resolve_remote("gpu")

    def test_named_remote_missing_url(self):
        with _patch_config({"remotes": {"gpu": {"node_id": "fp:x"}}}):
            with pytest.raises(ValueError, match="no url configured"):
                resolve_remote("gpu")

    def test_missing_node_id_returns_empty(self):
        """node_id is optional — empty string if not configured."""
        config = {"remotes": {"gpu": {"url": "https://gpu.example.com"}}}
        with _patch_config(config):
            rc = resolve_remote("gpu")
        assert rc.node_id == ""

    def test_malformed_entry_not_dict(self):
        config = {"remotes": {"gpu": "https://example.com"}}
        with _patch_config(config):
            with pytest.raises(ValueError, match="must be a mapping"):
                resolve_remote("gpu")

    def test_remotes_not_dict(self):
        config = {"remotes": "bad"}
        with _patch_config(config):
            with pytest.raises(ValueError, match="not found"):
                resolve_remote()

    def test_env_vars_ignored_without_config(self, monkeypatch):
        """Env vars alone are not sufficient — config must declare remotes."""
        monkeypatch.setenv("RYE_REMOTE_URL", "https://env.example.com")
        with _patch_config({}):
            with pytest.raises(ValueError, match="not found"):
                resolve_remote()


class TestGetProjectPath:

    def test_from_config(self):
        with _patch_config({"project_path": "my-project"}):
            assert get_project_path() == "my-project"

    def test_fallback_to_dirname(self, tmp_path):
        with _patch_config({}):
            assert get_project_path(tmp_path) == tmp_path.name

    def test_fallback_no_path(self):
        with _patch_config({}):
            assert get_project_path() == "unknown"

    def test_non_string_fallback(self, tmp_path):
        with _patch_config({"project_path": 123}):
            assert get_project_path(tmp_path) == tmp_path.name


class TestListRemotes:

    def test_lists_configured_remotes(self):
        config = {
            "remotes": {
                "default": {"url": "https://a.com", "node_id": "fp:abc"},
                "gpu": {"url": "https://b.com"},
            },
        }
        with _patch_config(config):
            result = list_remotes()
        assert result["default"]["node_id"] == "fp:abc"
        assert result["gpu"]["node_id"] == ""

    def test_empty_config_returns_empty(self):
        with _patch_config({}):
            result = list_remotes()
        assert result == {}

    def test_malformed_entry_skipped(self):
        config = {
            "remotes": {
                "bad": "string",
                "good": {"url": "https://x.com", "node_id": "fp:xyz"},
            },
        }
        with _patch_config(config):
            result = list_remotes()
        assert "bad" not in result
        assert "good" in result
        assert result["good"]["url"] == "https://x.com"


class TestRemoteToolIntegration:
    """Verify remote.py uses resolve_remote for client creation."""

    def test_get_client_uses_resolve_remote(self):
        """_get_client() calls resolve_remote() and creates RemoteHttpClient."""
        config = {"remotes": {"gpu": {"url": "https://gpu.example.com", "node_id": "fp:test123"}}}
        with _patch_config(config):
            import importlib
            import remote as remote_mod

            importlib.reload(remote_mod)
            client = remote_mod._get_client("gpu", None)
            assert client.base_url == "https://gpu.example.com"
            assert client.node_id == "fp:test123"


class TestParseEnvFile:
    """Tests for _parse_env_file — removed with Supabase secrets migration.

    The _parse_env_file function and all remote secret actions
    (secrets_push, secrets_list, secrets_remove) were removed in Phase 2
    (Sealed Secrets). Secrets are now managed locally via the secrets tool
    and sealed into HPKE envelopes before dispatch.
    """

    @pytest.mark.skip(reason="Removed: Supabase secrets replaced by sealed envelopes")
    def test_placeholder(self):
        pass


@pytest.mark.asyncio
class TestRemoteExecuteThreadValidation:
    """Tests for _execute thread validation in the remote tool."""

    def _load_remote(self):
        """Load remote.py module from bundle."""
        spec = importlib.util.spec_from_file_location("remote_exec_test", _REMOTE_PATH)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod

    async def test_missing_thread_returns_error(self):
        """_execute with no thread param → error."""
        mod = self._load_remote()
        result = await mod._execute(Path("/tmp/fake"), {
            "item_type": "tool",
            "item_id": "test/tool",
            "parameters": {},
        })
        assert "error" in result
        assert "thread" in result["error"].lower()

    async def test_empty_thread_returns_error(self):
        """_execute with thread='' → error."""
        mod = self._load_remote()
        result = await mod._execute(Path("/tmp/fake"), {
            "item_type": "tool",
            "item_id": "test/tool",
            "parameters": {},
            "thread": "",
        })
        assert "error" in result
        assert "thread" in result["error"].lower()

    async def test_missing_item_type_returns_error(self):
        """_execute with no item_type → error."""
        mod = self._load_remote()
        result = await mod._execute(Path("/tmp/fake"), {
            "item_id": "test/tool",
            "thread": "inline",
        })
        assert "error" in result
        assert "item_type" in result["error"]

    async def test_missing_item_id_returns_error(self):
        """_execute with no item_id → error."""
        mod = self._load_remote()
        result = await mod._execute(Path("/tmp/fake"), {
            "item_type": "tool",
            "thread": "inline",
        })
        assert "error" in result
        assert "item_id" in result["error"]
