# rye:signed:2026-02-26T03:49:32Z:8b7481ebb9ad0899972120afe8b91cc184d6a1c676a6112e080b298b74932480:ba928_203OoRSLuY-WErygfYRFGP1MBG_uydBx7MEWbMs7zpdx1cW5zagsxJMOcaMwgJTe2xsihmJOFPwDBsCg==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Thread configuration loader"

from pathlib import Path
from typing import Any, Dict, Optional

import yaml

from rye.constants import AI_DIR
from rye.utils.path_utils import get_user_ai_path, get_system_spaces


class ConfigLoader:
    """Base loader for YAML configs with extends support."""

    def __init__(self, config_name: str):
        self.config_name = config_name
        self._cache: Dict[str, Any] = {}

    def load(self, project_path: Path) -> Dict[str, Any]:
        """Load config with system (all bundles) → user → project cascade."""
        cache_key = str(project_path)
        if cache_key in self._cache:
            return self._cache[cache_key]

        # System defaults — merge from all bundles
        config: Dict[str, Any] = {}
        for bundle in get_system_spaces():
            system_path = bundle.root_path / AI_DIR / "tools" / "rye" / "agent" / "threads" / "config" / self.config_name
            if system_path.exists():
                bundle_config = self._load_yaml(system_path)
                config = self._merge(config, bundle_config)

        user_config_path = get_user_ai_path() / "config" / self.config_name
        if user_config_path.exists():
            user_config = self._load_yaml(user_config_path)
            config = self._merge(config, user_config)

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
