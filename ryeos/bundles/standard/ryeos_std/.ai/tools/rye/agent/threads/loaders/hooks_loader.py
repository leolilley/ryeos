# rye:signed:2026-02-26T05:02:40Z:33c8be87b5b827fe98ff4a27f24132f18404163fc6cdae89be3f4bd67d81e45a:9TDaWKteoN2jLJnnlgXN3rZXypfg7sIh7Z36p7nkqnAc5DtL7c6vU9v2IOUvBmCxFyJNeX6tVRP9U9aUFid4Cg==:4b987fd4e40303ac
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

    def get_context_hooks(self, project_path: Path) -> List[Dict]:
        config = self.load(project_path)
        return config.get("context_hooks", [])

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
