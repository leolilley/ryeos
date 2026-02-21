# rye:signed:2026-02-21T05:56:40Z:59f4d4f333e6272d96ab87f5b59884bdeef10e2d501770b54ac176a2c54ec962:7BkEJq3aWcB2VoLHEq1swj4oIRLanr_SYaGstK6ppGu2Gsej890C35G4ubtwLoUftC8ChEa6RjqvLtwMjieXCA==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Error classification loader"

from pathlib import Path
from typing import Any, Dict, Optional

from .config_loader import ConfigLoader
from .condition_evaluator import matches


class ErrorLoader(ConfigLoader):
    def __init__(self):
        super().__init__("error_classification.yaml")

    def classify(self, project_path: Path, error_context: Dict) -> Dict:
        """Classify an error based on config patterns."""
        config = self.load(project_path)

        for pattern in config.get("patterns", []):
            if matches(error_context, pattern.get("match", {})):
                return {
                    "category": pattern.get("category", "permanent"),
                    "retryable": pattern.get("retryable", False),
                    "retry_policy": pattern.get("retry_policy"),
                    "code": pattern.get("id"),
                }

        default = config.get("default", {})
        return {
            "category": default.get("category", "permanent"),
            "retryable": default.get("retryable", False),
        }

    def calculate_retry_delay(
        self, project_path: Path, policy: Dict, attempt: int
    ) -> float:
        policy_type = policy.get("type", "none")
        if policy_type == "exponential":
            base = policy.get("base", 2.0)
            max_delay = policy.get("max", 120.0)
            return min(base * (2**attempt), max_delay)
        if policy_type == "fixed":
            return policy.get("delay", 60.0)
        return 0.0


_error_loader: Optional[ErrorLoader] = None


def get_error_loader() -> ErrorLoader:
    global _error_loader
    if _error_loader is None:
        _error_loader = ErrorLoader()
    return _error_loader


def load(project_path: Path) -> Dict[str, Any]:
    return get_error_loader().load(project_path)


def classify(project_path: Path, error_context: Dict) -> Dict:
    return get_error_loader().classify(project_path, error_context)
