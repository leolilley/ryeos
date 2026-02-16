# rye:signed:2026-02-16T07:32:54Z:a748b6f85277c0bf5ed6a721ab0436e3363d5c45dcf1a97f74e65af4b806488e:X0edDd1oLtU5zTIyGrnq1PQaniEDfO848QxZfDoQUQ_AT7BxyUNtHqcAenqvrMViIHvrTQ8owvNz_vMwGtaVAA==:440443d0858f0199
"""
provider_resolver.py: Resolve model/tier to a concrete provider adapter.

Searches provider YAML configs in project → user → system space.
No hardcoded provider — if no config matches, raises ProviderNotFoundError.
"""

__version__ = "1.2.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/adapters"
__tool_description__ = "Provider resolver for thread execution"

import logging
import os
from pathlib import Path
from typing import Dict, List, Optional, Tuple

import yaml

from rye.constants import AI_DIR
from rye.utils.path_utils import get_user_space, get_system_space

logger = logging.getLogger(__name__)


class ProviderNotFoundError(Exception):
    """No provider config found for the requested model/tier."""
    pass


def _build_item_id(config: Dict, yaml_path: Path) -> str:
    """Build tool item_id from provider config (category/tool_id)."""
    category = config.get("category", "")
    tool_id = config.get("tool_id", yaml_path.stem)
    if category:
        return f"{category}/{tool_id}"
    return tool_id


def _get_provider_dirs(project_path: Optional[Path] = None) -> List[Path]:
    """Get provider config directories in precedence order: project → user → system."""
    dirs = []
    if project_path:
        p = project_path / AI_DIR / "tools" / "rye" / "agent" / "providers"
        if p.exists():
            dirs.append(p)
    user = get_user_space() / AI_DIR / "tools" / "rye" / "agent" / "providers"
    if user.exists():
        dirs.append(user)
    system = get_system_space() / AI_DIR / "tools" / "rye" / "agent" / "providers"
    if system.exists():
        dirs.append(system)
    if os.environ.get("RYE_DEBUG"):
        all_searched = []
        if project_path:
            all_searched.append(str(project_path / AI_DIR / "tools" / "rye" / "agent" / "providers"))
        all_searched.append(str(get_user_space() / AI_DIR / "tools" / "rye" / "agent" / "providers"))
        all_searched.append(str(get_system_space() / AI_DIR / "tools" / "rye" / "agent" / "providers"))
        logger.debug("Provider dirs searched: %s → found: %s", all_searched, [str(d) for d in dirs])
    return dirs


def _load_provider_configs(project_path: Optional[Path] = None) -> List[Tuple[Path, Dict]]:
    """Load all provider YAML configs from all spaces."""
    configs = []
    seen_ids = set()
    for provider_dir in _get_provider_dirs(project_path):
        for yaml_path in sorted(provider_dir.glob("*.yaml")):
            try:
                with open(yaml_path) as f:
                    config = yaml.safe_load(f) or {}
                tool_id = config.get("tool_id", yaml_path.stem)
                # Project configs shadow user/system configs with same tool_id
                if tool_id not in seen_ids:
                    configs.append((yaml_path, config))
                    seen_ids.add(tool_id)
            except Exception as e:
                logger.warning("Failed to load provider config %s: %s", yaml_path, e)
    return configs


def resolve_provider(
    model: str,
    project_path: Optional[Path] = None,
) -> Tuple[str, str, Dict]:
    """Resolve a model string to a concrete provider config.

    Resolution order:
    1. Check tier_mapping in each provider config (e.g., "haiku" → "claude-3-5-haiku-20241022")
    2. Check if model string matches a known model ID directly

    Args:
        model: Model tier name (e.g., "haiku") or full model ID (e.g., "claude-3-5-haiku-20241022")
        project_path: Project root for project-space provider discovery

    Returns:
        Tuple of (resolved_model_id, provider_item_id, provider_config_dict)

    Raises:
        ProviderNotFoundError: If no provider handles the requested model/tier.
    """
    configs = _load_provider_configs(project_path)

    if not configs:
        dirs = _get_provider_dirs(project_path)
        searched = ", ".join(str(d) for d in dirs) if dirs else "no provider directories found"
        raise ProviderNotFoundError(
            f"No provider configs found. Searched: {searched}. "
            f"Create a provider YAML at {AI_DIR}/tools/rye/agent/providers/"
        )

    # Pass 1: Check tier_mapping
    for yaml_path, config in configs:
        tier_mapping = config.get("tier_mapping", {})
        if model in tier_mapping:
            resolved_model = tier_mapping[model]
            item_id = _build_item_id(config, yaml_path)
            logger.debug(
                "Resolved tier '%s' → model '%s' via %s",
                model, resolved_model, yaml_path.name,
            )
            return resolved_model, item_id, config

    # Pass 2: Check if model is a known model ID in any tier_mapping values
    for yaml_path, config in configs:
        tier_mapping = config.get("tier_mapping", {})
        if model in tier_mapping.values():
            item_id = _build_item_id(config, yaml_path)
            logger.debug(
                "Matched model ID '%s' directly via %s",
                model, yaml_path.name,
            )
            return model, item_id, config

    # Pass 3: Check if model is a prefix of any known model ID
    for yaml_path, config in configs:
        tier_mapping = config.get("tier_mapping", {})
        for tier, model_id in tier_mapping.items():
            if model_id.startswith(model) or model.startswith(model_id):
                item_id = _build_item_id(config, yaml_path)
                logger.debug(
                    "Prefix-matched model '%s' → '%s' via %s",
                    model, model_id, yaml_path.name,
                )
                return model_id, item_id, config

    # No match
    available_tiers = {}
    for yaml_path, config in configs:
        provider_id = config.get("tool_id", yaml_path.stem)
        for tier, model_id in config.get("tier_mapping", {}).items():
            available_tiers[tier] = f"{model_id} ({provider_id})"

    tier_list = "\n".join(f"  - {tier}: {info}" for tier, info in sorted(available_tiers.items()))
    raise ProviderNotFoundError(
        f"No provider found for model '{model}'. "
        f"Available tiers:\n{tier_list}\n"
        f"Either use a known tier/model ID or add a provider config at "
        f"{AI_DIR}/tools/rye/agent/providers/"
    )


def list_providers(
    project_path: Optional[Path] = None,
) -> List[Dict]:
    """List all available providers with their tier mappings and models.

    Returns list of dicts, each with:
        provider_id: str — tool item_id
        tool_id: str — short name
        tiers: dict — tier → model_id mapping
        models: list — all model IDs this provider supports
        pricing: dict — per-model pricing info
    """
    configs = _load_provider_configs(project_path)
    providers = []
    for yaml_path, config in configs:
        item_id = _build_item_id(config, yaml_path)
        tier_mapping = config.get("tier_mapping", {})
        providers.append({
            "provider_id": item_id,
            "tool_id": config.get("tool_id", yaml_path.stem),
            "tool_type": config.get("tool_type", "http"),
            "tiers": tier_mapping,
            "models": list(set(tier_mapping.values())),
            "pricing": config.get("pricing", {}),
            "context_window": config.get("context_window"),
        })
    return providers
