"""Configuration for ryeos-remote server."""

from functools import lru_cache
from pathlib import Path

from pydantic import ConfigDict
from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    model_config = ConfigDict(env_file=".env", env_file_encoding="utf-8")

    # CAS storage
    cas_base_path: str = "/cas"

    # Remote signing key
    signing_key_dir: str = "/cas/signing"

    # Node config (authorized keys, node identity)
    node_config_dir: str = ""  # defaults to <cas_base_path>/config/

    # Remote identity (server-asserted, set via RYE_REMOTE_NAME env var)
    rye_remote_name: str = "default"

    # Server
    host: str = "0.0.0.0"
    port: int = 8000

    # Limits
    max_request_bytes: int = 50 * 1024 * 1024  # 50MB
    max_user_storage_bytes: int = 1024 * 1024 * 1024  # 1GB

    def _node_config(self) -> Path:
        if self.node_config_dir:
            return Path(self.node_config_dir)
        return Path(self.cas_base_path) / "config"

    def authorized_keys_dir(self) -> Path:
        return self._node_config() / "authorized_keys"

    def user_cas_root(self, fingerprint: str) -> Path:
        return Path(self.cas_base_path) / fingerprint / ".ai" / "objects"

    def cache_root(self, fingerprint: str) -> Path:
        return Path(self.cas_base_path) / fingerprint / "cache"

    def exec_root(self, fingerprint: str) -> Path:
        return Path(self.cas_base_path) / fingerprint / "executions"


@lru_cache
def get_settings() -> Settings:
    return Settings()
