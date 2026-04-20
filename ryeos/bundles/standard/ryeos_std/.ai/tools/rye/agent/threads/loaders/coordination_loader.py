# rye:signed:2026-04-19T09:49:53Z:686d46b3832cbfb6bbdf75765affb19b48abdf3c7f04e8795834e0fcf9372bf4:1OFmITqCPIe3ass+UF5K/1jrtrobi83n28XgcS7kCBDZv6lU79H4Xeod/mtq18x2cbRvESELt4JqWhECUvrXCw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Coordination config loader"

from pathlib import Path
from typing import Any, Dict, Optional

from module_loader import load_module

_ANCHOR = Path(__file__).parent

config_loader = load_module("config_loader", anchor=_ANCHOR)


class CoordinationLoader(config_loader.ConfigLoader):
    def __init__(self):
        super().__init__("coordination.yaml")

    def get_wait_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("coordination", {}).get("wait_threads", {})

    def get_continuation_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("coordination", {}).get("continuation", {})

    def get_orphan_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("coordination", {}).get("orphan_detection", {})


_loader: Optional[CoordinationLoader] = None


def get_coordination_loader() -> CoordinationLoader:
    global _loader
    if _loader is None:
        _loader = CoordinationLoader()
    return _loader


def load(project_path: Path) -> Dict[str, Any]:
    return get_coordination_loader().load(project_path)
