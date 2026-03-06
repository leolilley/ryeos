"""
Processor Router - Routes processing steps to data-driven processors.

Processors are loaded from .ai/tools/rye/core/processors/ (system) and can
be overridden by project or user space at .ai/processors/.

Each processor module exports a ``process(*args, **kwargs)`` function.
"""

import importlib.util
import logging
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR
from rye.utils.path_utils import get_user_space, get_system_spaces

logger = logging.getLogger(__name__)


class ProcessorRouter:
    """Routes processing requests to the appropriate data-driven processor."""

    def __init__(self, project_path: Optional[Path] = None):
        """Initialize processor router."""
        self.project_path = project_path
        self._processors: Dict[str, Any] = {}

    def get_search_paths(self) -> List[Path]:
        """Get processor search paths in precedence order."""
        paths = []

        # Project processors (highest priority)
        if self.project_path:
            project_processors = self.project_path / AI_DIR / "processors"
            if project_processors.exists():
                paths.append(project_processors)

        # User processors
        user_processors = get_user_space() / AI_DIR / "processors"
        if user_processors.exists():
            paths.append(user_processors)

        # System processors from all roots (lowest priority)
        for bundle in get_system_spaces():
            system_processors = (
                bundle.root_path / AI_DIR / "tools" / "rye" / "core" / "processors"
            )
            if system_processors.exists():
                paths.append(system_processors)

        return paths

    def _load_processor(self, processor_name: str) -> Optional[Any]:
        """Load a processor module by name."""
        if processor_name in self._processors:
            return self._processors[processor_name]

        for search_path in self.get_search_paths():
            processor_file = search_path / f"{processor_name}.py"
            if processor_file.exists():
                try:
                    spec = importlib.util.spec_from_file_location(
                        processor_name, processor_file
                    )
                    if spec and spec.loader:
                        module = importlib.util.module_from_spec(spec)
                        spec.loader.exec_module(module)
                        self._processors[processor_name] = module
                        logger.debug(
                            f"Loaded processor: {processor_name} from {processor_file}"
                        )
                        return module
                except Exception as e:
                    logger.warning(f"Failed to load processor {processor_name}: {e}")
                    continue

        logger.warning(f"Processor not found: {processor_name}")
        return None

    def run(self, processor_name: str, *args: Any, **kwargs: Any) -> Any:
        """
        Run a processor by name.

        Args:
            processor_name: Name of processor (e.g., "inputs/validate", "inputs/interpolate")
            *args: Positional arguments forwarded to the processor's process() function
            **kwargs: Keyword arguments forwarded to the processor's process() function

        Returns:
            Result from the processor, or dict with "error" key on failure
        """
        processor = self._load_processor(processor_name)
        if not processor:
            return {"error": f"Processor not found: {processor_name}"}

        if not hasattr(processor, "process"):
            return {"error": f"Processor {processor_name} has no process() function"}

        try:
            return processor.process(*args, **kwargs)
        except Exception as e:
            processor_path = getattr(processor, "__file__", "unknown")
            logger.error(
                "Processor %s (%s) failed: %s",
                processor_name,
                processor_path,
                e,
            )
            return {"error": f"{processor_name}: {e}"}

    def list_processors(self) -> List[str]:
        """List available processor names."""
        processors = set()
        for search_path in self.get_search_paths():
            for file_path in search_path.rglob("*.py"):
                if not file_path.name.startswith("_"):
                    rel = file_path.relative_to(search_path).with_suffix("")
                    processors.add(str(rel))
        return sorted(processors)
