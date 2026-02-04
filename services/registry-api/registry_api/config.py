"""Configuration settings for Registry API."""

from functools import lru_cache
from typing import List, Optional

from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    """Application settings loaded from environment variables."""

    # Supabase
    supabase_url: str
    supabase_service_key: str
    supabase_jwt_secret: str

    # Server
    host: str = "0.0.0.0"
    port: int = 8000
    log_level: str = "INFO"

    # CORS
    allowed_origins: List[str] = ["*"]

    # Rate limiting (requests per minute)
    rate_limit_push: int = 30
    rate_limit_pull: int = 100

    class Config:
        env_file = ".env"
        env_file_encoding = "utf-8"


@lru_cache
def get_settings() -> Settings:
    """Get cached settings instance."""
    return Settings()
