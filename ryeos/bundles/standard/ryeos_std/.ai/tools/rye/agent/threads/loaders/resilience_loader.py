# rye:signed:2026-04-10T00:57:19Z:4be0e4b9cd6a7015498b207ddea4dc13e2fa7e0d7bb5ff1f4c38e8cee339f6e3:SnjS0isJ-7seKfXXTz8oE7zNloCbm8R09WHi-QKTF3-8pN5hyBZ-enic3oFziOIDWgoHuOxXen5tus-I7K-MAw:4b987fd4e40303ac
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Resilience configuration loader"

from pathlib import Path
from typing import Any, Dict, Optional

from .config_loader import ConfigLoader


class ResilienceLoader(ConfigLoader):
    def __init__(self):
        super().__init__("resilience.yaml")

    def get_default_limits(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("limits", {}).get("defaults", {})

    def get_retry_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("retry", {})

    def get_coordination_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("coordination", {})

    def get_child_policy(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("child_policy", {})

    def get_tool_preload_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("tool_preload", {})


_resilience_loader: Optional[ResilienceLoader] = None


def get_resilience_loader() -> ResilienceLoader:
    global _resilience_loader
    if _resilience_loader is None:
        _resilience_loader = ResilienceLoader()
    return _resilience_loader


def load(project_path: Path) -> Dict[str, Any]:
    return get_resilience_loader().load(project_path)
