# rye:signed:2026-02-23T08:17:58Z:6a773ec0227358d57ae0887a8f5e56daa9920d6dcac84264d2acf646575c68e3:ZXfyvvwb5BvCnswqvL7jZac_y4l6JjuManrbDpstEkI2K4ispo1oqVqCktZuhNIm01GPju7kkpg2VGZk-2HbBA==:9fbfabe975fa5a7f
__version__ = "1.1.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Hooks configuration loader"

import yaml
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR
from rye.utils.path_utils import get_user_ai_path

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

    def get_user_hooks(self) -> List[Dict]:
        user_hooks_path = get_user_ai_path() / "config" / "agent" / "hooks.yaml"
        if user_hooks_path.exists():
            with open(user_hooks_path) as f:
                config = yaml.safe_load(f) or {}
            return config.get("hooks", [])
        return []

    def get_project_hooks(self, project_path: Path) -> List[Dict]:
        project_hooks_path = project_path / AI_DIR / "config" / "agent" / "hooks.yaml"
        if project_hooks_path.exists():
            with open(project_hooks_path) as f:
                config = yaml.safe_load(f) or {}
            return config.get("hooks", [])
        return []


_hooks_loader: Optional[HooksLoader] = None


def get_hooks_loader() -> HooksLoader:
    global _hooks_loader
    if _hooks_loader is None:
        _hooks_loader = HooksLoader()
    return _hooks_loader


def load(project_path: Path) -> Dict[str, Any]:
    return get_hooks_loader().load(project_path)
