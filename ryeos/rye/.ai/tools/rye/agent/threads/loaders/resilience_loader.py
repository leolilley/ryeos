# rye:signed:2026-02-21T05:56:40Z:ff669979264178fb019454d75c8a959175cdd382c398cf06c85599556799a0d8:ClklBTrSsmvZ6vAWB4xIzXUUCdmkm8WiXZlbbYzvpT_sRQjIRj4iX6Sw7jwa1NAuzjbtXFH3P_2PaYZCqrmSAg==:9fbfabe975fa5a7f
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


_resilience_loader: Optional[ResilienceLoader] = None


def get_resilience_loader() -> ResilienceLoader:
    global _resilience_loader
    if _resilience_loader is None:
        _resilience_loader = ResilienceLoader()
    return _resilience_loader


def load(project_path: Path) -> Dict[str, Any]:
    return get_resilience_loader().load(project_path)
