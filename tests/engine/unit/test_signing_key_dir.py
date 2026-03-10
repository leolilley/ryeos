"""Tests for get_signing_key_dir() helper."""

import os
from pathlib import Path
from unittest.mock import patch

from rye.utils.path_utils import get_signing_key_dir


class TestGetSigningKeyDir:
    """Test signing key directory resolution."""

    def test_default_falls_back_to_user_space(self):
        """Without env var, uses user space."""
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("RYE_SIGNING_KEY_DIR", None)
            result = get_signing_key_dir()
            assert result.parts[-3:] == ("config", "keys", "signing")

    def test_env_var_overrides(self):
        """RYE_SIGNING_KEY_DIR env var takes precedence."""
        with patch.dict(os.environ, {"RYE_SIGNING_KEY_DIR": "/custom/keys"}):
            result = get_signing_key_dir()
            assert result == Path("/custom/keys")

    def test_env_var_empty_string_falls_back(self):
        """Empty env var should fall back to default."""
        with patch.dict(os.environ, {"RYE_SIGNING_KEY_DIR": ""}):
            result = get_signing_key_dir()
            # Empty string is falsy, so should fall back
            assert result.parts[-3:] == ("config", "keys", "signing")
