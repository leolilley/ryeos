"""Configuration for ryeos-remote server."""

from functools import lru_cache
from pathlib import Path

from pydantic import ConfigDict
from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    model_config = ConfigDict(env_file=".env", env_file_encoding="utf-8")

    # Supabase (auth + secrets)
    supabase_url: str
    supabase_service_key: str
    supabase_jwt_secret: str

    # CAS storage
    cas_base_path: str = "/cas"

    # Remote signing key
    signing_key_dir: str = "/cas/signing"

    # Server
    host: str = "0.0.0.0"
    port: int = 8000

    # Limits
    max_request_bytes: int = 50 * 1024 * 1024  # 50MB
    max_user_storage_bytes: int = 1024 * 1024 * 1024  # 1GB

    def user_cas_root(self, user_id: str) -> Path:
        return Path(self.cas_base_path) / user_id / ".ai" / "objects"


@lru_cache
def get_settings() -> Settings:
    return Settings()
