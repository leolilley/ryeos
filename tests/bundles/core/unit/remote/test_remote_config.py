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
    get_project_name,
    list_remotes,
)


def _patch_config(config):
    """Patch _load_remote_config to return a fixed dict."""
    return patch("remote_config._load_remote_config", return_value=config)


class TestResolveRemote:
    """Tests for resolve_remote()."""

    def test_named_remote_from_config(self, monkeypatch):
        monkeypatch.setenv("RYE_GPU_API_KEY", "gpu-secret")
        config = {
            "remotes": {
                "gpu": {"url": "https://gpu.example.com", "key_env": "RYE_GPU_API_KEY"},
            },
        }
        with _patch_config(config):
            rc = resolve_remote("gpu")
        assert rc == RemoteConfig(name="gpu", url="https://gpu.example.com", api_key="gpu-secret")

    def test_default_remote_from_config(self, monkeypatch):
        monkeypatch.setenv("RYE_REMOTE_API_KEY", "default-key")
        config = {
            "remotes": {
                "default": {"url": "https://default.example.com", "key_env": "RYE_REMOTE_API_KEY"},
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
        with _patch_config({"remotes": {"default": {"url": "x", "key_env": "K"}}}):
            with pytest.raises(ValueError, match="'gpu' not found"):
                resolve_remote("gpu")

    def test_named_remote_missing_url(self, monkeypatch):
        monkeypatch.setenv("K", "val")
        with _patch_config({"remotes": {"gpu": {"key_env": "K"}}}):
            with pytest.raises(ValueError, match="no url configured"):
                resolve_remote("gpu")

    def test_named_remote_missing_key_env(self, monkeypatch):
        monkeypatch.delenv("MY_KEY", raising=False)
        config = {"remotes": {"gpu": {"url": "https://gpu.example.com", "key_env": "MY_KEY"}}}
        with _patch_config(config):
            with pytest.raises(ValueError, match="MY_KEY"):
                resolve_remote("gpu")

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
        monkeypatch.setenv("RYE_REMOTE_API_KEY", "env-key")
        with _patch_config({}):
            with pytest.raises(ValueError, match="not found"):
                resolve_remote()


class TestGetProjectName:

    def test_from_config(self):
        with _patch_config({"project_name": "my-project"}):
            assert get_project_name() == "my-project"

    def test_fallback_to_dirname(self, tmp_path):
        with _patch_config({}):
            assert get_project_name(tmp_path) == tmp_path.name

    def test_fallback_no_path(self):
        with _patch_config({}):
            assert get_project_name() == "unknown"

    def test_non_string_fallback(self, tmp_path):
        with _patch_config({"project_name": 123}):
            assert get_project_name(tmp_path) == tmp_path.name


class TestListRemotes:

    def test_lists_configured_remotes(self, monkeypatch):
        monkeypatch.setenv("K1", "val")
        monkeypatch.delenv("K2", raising=False)
        config = {
            "remotes": {
                "default": {"url": "https://a.com", "key_env": "K1"},
                "gpu": {"url": "https://b.com", "key_env": "K2"},
            },
        }
        with _patch_config(config):
            result = list_remotes()
        assert result["default"]["key_set"] is True
        assert result["gpu"]["key_set"] is False

    def test_empty_config_returns_empty(self):
        with _patch_config({}):
            result = list_remotes()
        assert result == {}

    def test_malformed_entry_skipped(self, monkeypatch):
        monkeypatch.delenv("K", raising=False)
        config = {
            "remotes": {
                "bad": "string",
                "good": {"url": "https://x.com", "key_env": "K"},
            },
        }
        with _patch_config(config):
            result = list_remotes()
        assert "bad" not in result
        assert "good" in result
        assert result["good"]["url"] == "https://x.com"


class TestRemoteToolIntegration:
    """Verify remote.py uses resolve_remote for client creation."""

    def test_get_client_uses_resolve_remote(self, monkeypatch):
        """_get_client() calls resolve_remote() and creates RemoteHttpClient."""
        monkeypatch.setenv("MY_KEY", "test-key")
        config = {"remotes": {"gpu": {"url": "https://gpu.example.com", "key_env": "MY_KEY"}}}
        with _patch_config(config):
            import importlib
            import remote as remote_mod

            importlib.reload(remote_mod)
            client = remote_mod._get_client("gpu", None)
            assert client.base_url == "https://gpu.example.com"
            assert client.api_key == "test-key"


class TestParseEnvFile:
    """Tests for _parse_env_file in the remote tool."""

    def _load_remote(self):
        spec = importlib.util.spec_from_file_location("remote_env_test", _REMOTE_PATH)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod

    def test_parses_key_value_pairs(self, tmp_path):
        mod = self._load_remote()
        env_file = tmp_path / ".env"
        env_file.write_text("KEY1=value1\nKEY2=value2\n")
        result = mod._parse_env_file(env_file)
        assert result == {"KEY1": "value1", "KEY2": "value2"}

    def test_skips_comments_and_blanks(self, tmp_path):
        mod = self._load_remote()
        env_file = tmp_path / ".env"
        env_file.write_text("# comment\n\nKEY=val\n  \n# another\n")
        result = mod._parse_env_file(env_file)
        assert result == {"KEY": "val"}

    def test_skips_empty_values(self, tmp_path):
        mod = self._load_remote()
        env_file = tmp_path / ".env"
        env_file.write_text("EMPTY=\nGOOD=ok\n")
        result = mod._parse_env_file(env_file)
        assert result == {"GOOD": "ok"}

    def test_handles_equals_in_value(self, tmp_path):
        mod = self._load_remote()
        env_file = tmp_path / ".env"
        env_file.write_text("KEY=val=with=equals\n")
        result = mod._parse_env_file(env_file)
        assert result == {"KEY": "val=with=equals"}


@pytest.mark.asyncio
class TestRemoteSecretsActions:
    """Tests for secrets_push, secrets_list, secrets_remove validation."""

    def _load_remote(self):
        spec = importlib.util.spec_from_file_location("remote_secrets_test", _REMOTE_PATH)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod

    async def test_secrets_push_requires_input(self):
        """_secrets_push with no env_file or names → error."""
        mod = self._load_remote()
        result = await mod._secrets_push(Path("/tmp/fake"), {})
        assert "error" in result
        assert "env_file" in result["error"]

    async def test_secrets_push_file_not_found(self):
        """_secrets_push with nonexistent env_file → error."""
        mod = self._load_remote()
        result = await mod._secrets_push(Path("/tmp/fake"), {
            "env_file": "/tmp/nonexistent/.env",
        })
        assert "error" in result
        assert "not found" in result["error"].lower()

    async def test_secrets_push_no_set_env_vars(self, monkeypatch):
        """_secrets_push with names but no env vars set → error."""
        mod = self._load_remote()
        monkeypatch.delenv("UNSET_VAR_1", raising=False)
        monkeypatch.delenv("UNSET_VAR_2", raising=False)
        result = await mod._secrets_push(Path("/tmp/fake"), {
            "names": ["UNSET_VAR_1", "UNSET_VAR_2"],
        })
        assert "error" in result
        assert "No secrets found" in result["error"]

    async def test_secrets_remove_requires_name(self):
        """_secrets_remove with no secret_name → error."""
        mod = self._load_remote()
        result = await mod._secrets_remove(Path("/tmp/fake"), {})
        assert "error" in result
        assert "secret_name" in result["error"]


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
