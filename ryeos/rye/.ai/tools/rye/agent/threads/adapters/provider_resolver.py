# rye:signed:2026-02-23T00:42:51Z:0e215b754fb18b5583cf8670f4bd462f68b9d21ad7667f2c0af744b19b8f2306:1Avv0rj6qLfTkQvAxkkAUKbD9wa-W4xmgmjhiIIJWPTV1JdWVwCU79LRGFf7Pki66PHe-lR_Sj-omOzBJaHjCg==:9fbfabe975fa5a7f
"""
provider_resolver.py: Resolve model/tier to a concrete provider adapter.

Searches provider YAML configs in project → user → system space.
Supports provider hints from directives and default_provider from agent config.

Resolution priority:
1. Explicit provider hint (from <model provider="openai" /> in directive)
2. default_provider from agent config ({USER_SPACE}/.ai/config/agent/agent.yaml)
3. All providers in alphabetical order (first match wins)
"""

__version__ = "1.3.0"
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

# Agent config is cached per-process
_agent_config_cache: Dict[str, Dict] = {}


class ProviderNotFoundError(Exception):
    """No provider config found for the requested model/tier."""
    pass


def _load_agent_config(project_path: Optional[Path] = None) -> Dict:
    """Load agent config with 3-tier merge: system → user → project.

    Config paths follow .ai/ conventions:
        system: {system_space}/.ai/config/agent/agent.yaml
        user:   {USER_SPACE}/.ai/config/agent/agent.yaml
        project: {project}/.ai/config/agent/agent.yaml
    """
    cache_key = str(project_path or "")
    if cache_key in _agent_config_cache:
        return _agent_config_cache[cache_key]

    config: Dict = {}

    # System defaults (shipped with rye)
    system_config = get_system_space() / AI_DIR / "config" / "agent" / "agent.yaml"
    if system_config.exists():
        with open(system_config) as f:
            config = yaml.safe_load(f) or {}

    # User overrides
    user_config = get_user_space() / AI_DIR / "config" / "agent" / "agent.yaml"
    if user_config.exists():
        with open(user_config) as f:
            user = yaml.safe_load(f) or {}
        config = _deep_merge(config, user)

    # Project overrides
    if project_path:
        project_config = project_path / AI_DIR / "config" / "agent" / "agent.yaml"
        if project_config.exists():
            with open(project_config) as f:
                proj = yaml.safe_load(f) or {}
            config = _deep_merge(config, proj)

    _agent_config_cache[cache_key] = config
    return config


def _deep_merge(base: Dict, override: Dict) -> Dict:
    """Deep merge override into base."""
    result = dict(base)
    for key, value in override.items():
        if key in result and isinstance(result[key], dict) and isinstance(value, dict):
            result[key] = _deep_merge(result[key], value)
        else:
            result[key] = value
    return result


def _apply_model_profiles(config: Dict, model_id: str) -> Dict:
    """If config has profiles, find matching profile and deep-merge over base.

    Profiles allow a single provider YAML to support multiple API formats
    by matching model IDs to config/schema overrides. For example, a Zen
    gateway provider can route claude-* to Anthropic format and gemini-* to
    Google format, all from one YAML file.

    Match patterns use fnmatch glob syntax (e.g., "claude-*", "gemini-*").
    """
    profiles = config.get("profiles")
    if not profiles:
        return config

    import fnmatch

    for profile_name, profile in profiles.items():
        patterns = profile.get("match", [])
        for pattern in patterns:
            if fnmatch.fnmatch(model_id, pattern):
                merged = dict(config)
                merged.pop("profiles", None)
                for key in ("config", "tool_use", "pricing"):
                    if key in profile:
                        merged[key] = _deep_merge(merged.get(key, {}), profile[key])
                logger.debug(
                    "Applied profile '%s' for model '%s'", profile_name, model_id,
                )
                return merged

    return config


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
        for yaml_path in sorted(provider_dir.rglob("*.yaml")):
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


def _order_configs(
    configs: List[Tuple[Path, Dict]],
    preferred_provider: Optional[str],
) -> List[Tuple[Path, Dict]]:
    """Reorder configs so preferred provider is checked first."""
    if not preferred_provider:
        return configs
    preferred = []
    rest = []
    for yaml_path, config in configs:
        tool_id = config.get("tool_id", yaml_path.stem)
        if tool_id == preferred_provider:
            preferred.append((yaml_path, config))
        else:
            rest.append((yaml_path, config))
    return preferred + rest


def resolve_provider(
    model: str,
    project_path: Optional[Path] = None,
    provider: Optional[str] = None,
) -> Tuple[str, str, Dict]:
    """Resolve a model string to a concrete provider config.

    Resolution priority for provider selection:
    1. Explicit provider hint (from directive <model provider="..." />)
    2. default_provider from agent config
    3. All providers alphabetically (first match wins)

    Within the selected provider(s), resolution order:
    1. Check tier_mapping keys (e.g., "fast" → "claude-haiku-4-5-20251001")
    2. Check literal model ID match
    3. Check prefix match on model IDs

    Args:
        model: Model tier name (e.g., "fast") or full model ID
        project_path: Project root for project-space provider discovery
        provider: Explicit provider hint (e.g., "openai", "anthropic")

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

    # Determine preferred provider: explicit hint > agent config > none
    preferred = provider
    if not preferred:
        agent_config = _load_agent_config(project_path)
        preferred = agent_config.get("provider", {}).get("default")

    # If we have an explicit provider hint (from directive), filter to only that provider
    # Supports both tool_id ("zen_openai") and path-style ("zen/zen_openai")
    if provider:
        def _matches_provider(yaml_path: Path, config: Dict, hint: str) -> bool:
            tool_id = config.get("tool_id", yaml_path.stem)
            if tool_id == hint:
                return True
            # Path-style match: "zen/zen_openai" matches category suffix or relative path
            item_id = _build_item_id(config, yaml_path)
            return item_id.endswith(f"/{hint}") or item_id == hint

        filtered = [(p, c) for p, c in configs if _matches_provider(p, c, provider)]
        if not filtered:
            available = [c.get("tool_id", p.stem) for p, c in configs]
            raise ProviderNotFoundError(
                f"Provider '{provider}' not found. Available: {', '.join(available)}"
            )
        ordered = filtered
    else:
        ordered = _order_configs(configs, preferred)

    # Pass 1: Check tier_mapping
    # If no preferred provider, check for ambiguity first
    if not preferred:
        matches = [
            (p, c) for p, c in ordered
            if model in c.get("tier_mapping", {})
        ]
        if len(matches) > 1:
            providers = [c.get("tool_id", p.stem) for p, c in matches]
            raise ProviderNotFoundError(
                f"Multiple providers offer tier '{model}': {', '.join(providers)}. "
                f"Set provider.default in .ai/config/agent/agent.yaml or use "
                f'<model tier="{model}" provider="..." /> in the directive.'
            )

    for yaml_path, config in ordered:
        tier_mapping = config.get("tier_mapping", {})
        if model in tier_mapping:
            resolved_model = tier_mapping[model]
            item_id = _build_item_id(config, yaml_path)
            logger.debug(
                "Resolved tier '%s' → model '%s' via %s",
                model, resolved_model, yaml_path.name,
            )
            return resolved_model, item_id, _apply_model_profiles(config, resolved_model)

    # Pass 2: Check if model is a known model ID in any tier_mapping values
    for yaml_path, config in ordered:
        tier_mapping = config.get("tier_mapping", {})
        if model in tier_mapping.values():
            item_id = _build_item_id(config, yaml_path)
            logger.debug(
                "Matched model ID '%s' directly via %s",
                model, yaml_path.name,
            )
            return model, item_id, _apply_model_profiles(config, model)

    # Pass 3: Check if model is a prefix of any known model ID
    for yaml_path, config in ordered:
        tier_mapping = config.get("tier_mapping", {})
        for tier, model_id in tier_mapping.items():
            if model_id.startswith(model) or model.startswith(model_id):
                item_id = _build_item_id(config, yaml_path)
                logger.debug(
                    "Prefix-matched model '%s' → '%s' via %s",
                    model, model_id, yaml_path.name,
                )
                return model_id, item_id, _apply_model_profiles(config, model_id)

    # Pass 4: Check if model is a known model ID in pricing section
    for yaml_path, config in ordered:
        pricing = config.get("pricing", {})
        if model in pricing:
            item_id = _build_item_id(config, yaml_path)
            logger.debug(
                "Matched model ID '%s' via pricing in %s",
                model, yaml_path.name,
            )
            return model, item_id, _apply_model_profiles(config, model)

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
