# rye:signed:2026-02-22T09:00:56Z:f8e47d9ed253bf2acb4ea73760324a546d805e9b786e9dfe367605e4afc2aa58:8crVUS455cIqKvYw_23i9LsxvGVNa0v2o4lug3OyRFtLqUYX2VSV-KH-_fCz8tvg1WWBn2cW_hSRU2cJ2QG6Cg==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Hooks configuration loader"

from pathlib import Path
from typing import Any, Dict, List, Optional

from .config_loader import ConfigLoader


class HooksLoader(ConfigLoader):
    def __init__(self):
        super().__init__("hook_conditions.yaml")

    def get_builtin_hooks(self, project_path: Path) -> List[Dict]:
        config = self.load(project_path)
        return config.get("builtin_hooks", [])

    def get_infra_hooks(self, project_path: Path) -> List[Dict]:
        config = self.load(project_path)
        return config.get("infra_hooks", [])



_hooks_loader: Optional[HooksLoader] = None


def get_hooks_loader() -> HooksLoader:
    global _hooks_loader
    if _hooks_loader is None:
        _hooks_loader = HooksLoader()
    return _hooks_loader


def load(project_path: Path) -> Dict[str, Any]:
    return get_hooks_loader().load(project_path)
