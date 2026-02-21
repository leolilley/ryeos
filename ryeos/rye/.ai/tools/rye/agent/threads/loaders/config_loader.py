# rye:signed:2026-02-21T05:56:40Z:f4671e9ecea10f2a35fbcfd0c5107a30b628463b5975fce240788fce0cdf9ba2:WgQYWXET2gxeSq3jLPLMR2sFxdIiUiU2zb3vM2z-73cZoqLL8FiivRsnE1aA0quVmdrb0YOSUiqrZl6C58lkDw==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Thread configuration loader"

from pathlib import Path
from typing import Any, Dict, Optional

import yaml

from rye.constants import AI_DIR


class ConfigLoader:
    """Base loader for YAML configs with extends support."""

    def __init__(self, config_name: str):
        self.config_name = config_name
        self._cache: Dict[str, Any] = {}

    def load(self, project_path: Path) -> Dict[str, Any]:
        """Load config with project overrides."""
        cache_key = str(project_path)
        if cache_key in self._cache:
            return self._cache[cache_key]

        system_path = Path(__file__).parent.parent / "config" / self.config_name
        config = self._load_yaml(system_path)

        project_config_path = project_path / AI_DIR / "config" / self.config_name
        if project_config_path.exists():
            project_config = self._load_yaml(project_config_path)
            config = self._merge(config, project_config)

        self._cache[cache_key] = config
        return config

    def _load_yaml(self, path: Path) -> Dict[str, Any]:
        with open(path) as f:
            return yaml.safe_load(f) or {}

    def _merge(self, base: Dict, override: Dict) -> Dict:
        """Deep merge override into base.

        Merge semantics:
        - `extends` key: skipped (metadata only)
        - Dicts: recursive deep merge
        - Lists of dicts with `id` keys: merge-by-id
        - Lists without `id` keys: replace entirely
        - Scalars: replace
        """
        result = dict(base)
        for key, value in override.items():
            if key == "extends":
                continue
            if (
                key in result
                and isinstance(result[key], dict)
                and isinstance(value, dict)
            ):
                result[key] = self._merge(result[key], value)
            elif (
                key in result
                and isinstance(result[key], list)
                and isinstance(value, list)
                and result[key]
                and isinstance(result[key][0], dict)
                and result[key][0].get("id") is not None
            ):
                result[key] = self._merge_list_by_id(result[key], value)
            else:
                result[key] = value
        return result

    def _merge_list_by_id(self, base_list: list, override_list: list) -> list:
        """Merge two lists of dicts by their `id` field."""
        base_by_id = {
            item["id"]: item
            for item in base_list
            if isinstance(item, dict) and "id" in item
        }
        seen_ids = set()

        result = []
        for item in base_list:
            item_id = item.get("id") if isinstance(item, dict) else None
            if item_id is not None:
                override_item = next(
                    (
                        o
                        for o in override_list
                        if isinstance(o, dict) and o.get("id") == item_id
                    ),
                    None,
                )
                if override_item:
                    result.append(override_item)
                    seen_ids.add(item_id)
                else:
                    result.append(item)
                    seen_ids.add(item_id)
            else:
                result.append(item)

        for item in override_list:
            item_id = item.get("id") if isinstance(item, dict) else None
            if item_id is not None and item_id not in seen_ids:
                result.append(item)

        return result

    def clear_cache(self):
        self._cache.clear()
